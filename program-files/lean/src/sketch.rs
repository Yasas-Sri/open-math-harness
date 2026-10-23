//! `analyze`: the sketch analysis of task-scoring protocol v3.
//! The procedure is part of the protocol, so every detail below is
//! deliberate: changing one is a new [`ANALYSIS_SUITE_ID`].
//!
//! On the resident REPL, for one `sorries` candidate:
//!
//! 1. the candidate (header imports blanked, positions its own) is elaborated
//!    once for real — the status, and the environment `env1` in which the
//!    theorem and its auxiliaries (`match_N`, …) exist;
//! 2. a **renamed copy** (`<thm>_sketch`, padded so its positions are the
//!    candidate's) is elaborated in `env1` with `allTactics`, because the REPL
//!    builds a tactic's proof state in the environment *before* the command:
//!    the copy's states see the theorem's own auxiliaries and the verbatim
//!    frozen target elaborates in them (a restatement is caught);
//! 3. **closed steps** are the maximal sorry-free `have`/`obtain … : T := …`
//!    tactics and `·` blocks; each is *trivial* when a tactic of
//!    [`ANALYSIS_SUITE`] (in order, under [`ANALYSIS_HEARTBEATS`]) or the
//!    library search [`LIBRARY_SEARCH`] closes its statement from its
//!    context, and *unused* when Lean's unused-variable linter, on a copy
//!    whose sorries are an analysis-only axiom `leaf_hole`, reports every
//!    name it binds (unused steps become `skip` and the copy is re-linted,
//!    at most [`LINT_ROUNDS`] times; `this` and `_` count as used);
//! 4. each **leaf** (remaining `sorry`) is *hollow* when the frozen target
//!    follows from the leaf's statement under the suite: sketch-introduced
//!    locals are cleared newest first (what `clear` refuses is reverted into
//!    the statement), then `Iff.mp` against the verbatim frozen target opens
//!    the goal `H : <leaf> ⊢ <frozen target>` for the suite; when the frozen
//!    target no longer elaborates in the leaf (a root binder substituted or
//!    cased) the closed leaf statement is re-elaborated as text beside the
//!    frozen theorem instead (the pretty-printer options of [`PP_OPTIONS`]
//!    make printed goals re-parse); a leaf whose goal is not a proposition
//!    is `Untestable`, and so is one whose stripped statement the suite
//!    *refutes* — everything follows from a false statement, so the test
//!    decides nothing there (the vacuity control of [`hollowness`]).
//!
//! Failure: a REPL death or the wall-clock backstop [`ANALYZE_WALL`] ends the
//! analysis with `error` set and the fields obtained so far kept, never a
//! fabricated verdict. The analysis-only environments (`leaf_hole`, the
//! renamed copies) are children of `env1` that nothing else ever sees.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::time::{Duration, Instant};

use nucleus_lean_interface::{
    ANALYSIS_HEARTBEATS, ANALYSIS_SUITE, ANALYSIS_SUITE_ID, AnalyzeResponse, CheckStatus, LIBRARY_SEARCH,
    Leaf, LeafRoute, LeafVerdict, MAX_ANALYSIS_ITEMS, MAX_ANALYSIS_TEXT_CHARS, SketchCounts, Step, StepKind,
};

use crate::repl::{CommandReply, Message, Repl, Sorry, TacticInfo, TacticReply};
use crate::{bounded, status_of};

/// Wall-clock budget of one `analyze` request: a backstop against a hung
/// REPL (every tactic is already bounded by the deterministic heartbeat cap).
/// On expiry the pending request is killed with the REPL and the reply
/// carries `error`.
pub const ANALYZE_WALL: Duration = Duration::from_secs(120);
/// Most lint rounds (unused steps replaced by `skip`, copy re-linted).
pub const LINT_ROUNDS: usize = 4;
/// Pretty-printer options under which printed goals re-parse as terms:
/// binder types in `∃`, coercion types, `let` values instead of `⋯`, no
/// Mathlib binder-predicate sugar.
pub const PP_OPTIONS: &str = "set_option pp.funBinderTypes true\nset_option pp.coercions.types true\n\
                              set_option pp.showLetValues true\nset_option pp.mathlib.binderPredicates false";
/// The analysis-only lint environment: the tactic-aware unused-variable
/// linter plus the axiom every `sorry` of the copy becomes (a declaration
/// with `sorry` is never linted).
const LINT_ENV: &str =
    "set_option linter.unusedVariables.analyzeTactics true\naxiom leaf_hole.{u} {α : Sort u} : α";
/// Longest excerpt of a Lean message kept in a note.
const NOTE_CHARS: usize = 160;

// ---- Driving the REPL under a budget ----

/// Why a procedure stopped before finishing.
#[derive(Debug)]
pub(crate) enum Abort {
    /// The wall-clock budget is spent (the REPL may still be alive).
    Budget(Duration),
    /// The REPL died, was killed for not answering in time, or broke the
    /// protocol (the message).
    Repl(String),
    /// The procedure cannot run on this candidate (the reason).
    Failed(String),
}

impl Abort {
    pub(crate) fn message(&self) -> String {
        match self {
            Abort::Budget(budget) => format!("wall-clock budget of {} s exhausted", budget.as_secs()),
            Abort::Repl(why) | Abort::Failed(why) => why.clone(),
        }
    }
}

/// A REPL plus a deadline: every request gets the smaller of the remaining
/// budget and `per_request`, and an exhausted budget refuses to send.
pub(crate) struct Driver<'a> {
    repl: &'a mut Repl,
    started: Instant,
    budget: Duration,
    per_request: Duration,
}

impl<'a> Driver<'a> {
    pub(crate) fn new(repl: &'a mut Repl, budget: Duration, per_request: Duration) -> Self {
        Self { repl, started: Instant::now(), budget, per_request }
    }

    pub(crate) fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    fn timeout(&self) -> Result<Duration, Abort> {
        let left = self.budget.saturating_sub(self.started.elapsed());
        if left.is_zero() {
            return Err(Abort::Budget(self.budget));
        }
        Ok(left.min(self.per_request))
    }

    pub(crate) fn command(
        &mut self,
        source: &str,
        env: u64,
        all_tactics: bool,
    ) -> Result<CommandReply, Abort> {
        let timeout = self.timeout()?;
        self.repl
            .command_with(source, Some(env), all_tactics, timeout)
            .map_err(|e| Abort::Repl(e.to_string()))
    }

    pub(crate) fn tactic(&mut self, tactic: &str, proof_state: u64) -> Result<TacticReply, Abort> {
        let timeout = self.timeout()?;
        self.repl.tactic_within(tactic, proof_state, timeout).map_err(|e| Abort::Repl(e.to_string()))
    }
}

pub(crate) fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

fn excerpt(s: &str) -> String {
    s.chars().take(NOTE_CHARS).collect()
}

// ---- Text utilities (pure) ----

/// A position as Lean reports it: 1-based line, 0-based column in
/// codepoints. Ordered by line, then column.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Pos {
    pub line: u64,
    pub column: u64,
}

impl Pos {
    pub fn of(line: u64, column: u64) -> Self {
        Self { line, column }
    }
}

/// `start <= p < end`.
fn inside(p: Pos, span: (Pos, Pos)) -> bool {
    span.0 <= p && p < span.1
}

/// The byte offset of `pos` in `text` (the end of the line when the column
/// runs past it, the end of the text when the line does).
pub fn byte_offset(text: &str, pos: Pos) -> usize {
    let mut offset = 0;
    for (i, line) in text.split('\n').enumerate() {
        if i as u64 + 1 == pos.line {
            let column = usize::try_from(pos.column).unwrap_or(usize::MAX);
            return offset + line.char_indices().nth(column).map_or(line.len(), |(b, _)| b);
        }
        offset += line.len() + 1;
    }
    text.len()
}

/// `text` with `old` at `pos` replaced by `new`; `None` when `old` is not
/// there.
pub fn replace_at(text: &str, pos: Pos, old: &str, new: &str) -> Option<String> {
    let a = byte_offset(text, pos);
    let rest = text.get(a..)?;
    if !rest.starts_with(old) {
        return None;
    }
    let mut out = String::with_capacity(text.len() + new.len());
    out.push_str(text.get(..a)?);
    out.push_str(new);
    out.push_str(rest.get(old.len()..)?);
    Some(out)
}

/// The first byte index at bracket depth 0 where `sep` starts.
pub fn split_top(text: &str, sep: &str) -> Option<usize> {
    let mut depth = 0i64;
    for (i, c) in text.char_indices() {
        match c {
            '(' | '[' | '{' | '⟨' => depth += 1,
            ')' | ']' | '}' | '⟩' => depth -= 1,
            _ if depth == 0 && text.get(i..).is_some_and(|t| t.starts_with(sep)) => return Some(i),
            _ => {}
        }
    }
    None
}

/// A `have`/`obtain` tactic read as a step: `have <pat> : <type> := <rest>`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StepText {
    pub kind: StepKind,
    /// The bound name or pattern; `this` when none.
    pub name: String,
    pub statement: String,
    /// The proof is a `by` block (else a term).
    pub by_mode: bool,
}

/// Parse a tactic's text as a step. `None` for anything else, including a
/// `have`/`obtain` without a stated type (its statement cannot be tested).
pub fn parse_step(tactic: &str) -> Option<StepText> {
    let (kind, body) = if let Some(b) = tactic.strip_prefix("have") {
        (StepKind::Have, b)
    } else if let Some(b) = tactic.strip_prefix("obtain") {
        (StepKind::Obtain, b)
    } else {
        return None;
    };
    if !matches!(body.chars().next(), Some(' ' | '\n' | ':' | '⟨')) {
        return None;
    }
    let i = split_top(body, ":=")?;
    let head = body.get(..i)?;
    let rest = body.get(i + 2..)?.trim();
    let j = split_top(head, ":")?;
    let pat = head.get(..j)?.trim();
    let statement = head.get(j + 1..)?.trim().to_owned();
    let by_mode = rest.starts_with("by")
        && rest.get(2..).and_then(|r| r.chars().next()).is_none_or(|c| !(c.is_alphanumeric() || c == '_'));
    Some(StepText {
        kind,
        name: if pat.is_empty() { "this".to_owned() } else { pat.to_owned() },
        statement,
        by_mode,
    })
}

fn indent_of(line: &str) -> usize {
    line.chars().count() - line.trim_start().chars().count()
}

/// The spans of the `·` blocks of `text`: from the bullet to the last
/// non-blank line indented deeper than it.
pub fn bullets(text: &str) -> Vec<(Pos, Pos)> {
    let lines: Vec<&str> = text.split('\n').collect();
    let mut out = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if !line.trim_start().starts_with('·') {
            continue;
        }
        let ind = indent_of(line);
        let mut j = i + 1;
        while lines.get(j).is_some_and(|l| l.trim().is_empty() || indent_of(l) > ind) {
            j += 1;
        }
        while j > i + 1 && lines.get(j - 1).is_some_and(|l| l.trim().is_empty()) {
            j -= 1;
        }
        let last = lines.get(j - 1).map_or(0, |l| l.chars().count());
        out.push((Pos::of(i as u64 + 1, ind as u64), Pos::of(j as u64, last as u64)));
    }
    out
}

/// Whether the `sorry` at `pos` is a tactic (it starts a tactic line whose
/// predecessor does not end in `:=`, or follows `by`, `·`, `=>`, `;` or
/// `<;>` on its line) rather than a term (`:= sorry`, `exact sorry`,
/// `⟨sorry, _⟩`).
pub fn sorry_is_tactic(text: &str, pos: Pos) -> bool {
    let lines: Vec<&str> = text.split('\n').collect();
    let li = usize::try_from(pos.line.saturating_sub(1)).unwrap_or(usize::MAX);
    let line = lines.get(li).copied().unwrap_or_default();
    let before: String = line.chars().take(usize::try_from(pos.column).unwrap_or(usize::MAX)).collect();
    let before = before.trim_end();
    if before.is_empty() {
        let prev = lines
            .get(..li)
            .and_then(|earlier| earlier.iter().rev().find(|l| !l.trim().is_empty()))
            .map_or("", |l| l.trim_end());
        return !prev.ends_with(":=");
    }
    ["by", "·", "=>", ";", "<;>"].iter().any(|k| before.ends_with(k))
}

/// A goal as the REPL prints it, split into its parts; a leading `case` line is skipped.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Goal {
    /// Each hypothesis line: the names it declares and their type (as
    /// printed, `let` values included; continuation lines joined).
    pub hyps: Vec<(Vec<String>, String)>,
    pub target: Option<String>,
}

impl Goal {
    fn names(&self) -> impl Iterator<Item = &String> {
        self.hyps.iter().flat_map(|(names, _)| names.iter())
    }
}

pub fn parse_goal(goal: &str) -> Goal {
    let mut lines = goal.split('\n').peekable();
    if lines.peek().is_some_and(|l| l.starts_with("case ")) {
        lines.next();
    }
    let mut out = Goal { hyps: vec![], target: None };
    enum Cur {
        None,
        Target,
        Hyp,
    }
    let mut cur = Cur::None;
    for line in lines {
        if let Some(t) = line.strip_prefix('⊢') {
            out.target = Some(t.trim().to_owned());
            cur = Cur::Target;
            continue;
        }
        if matches!(cur, Cur::Target) {
            if let Some(t) = out.target.as_mut() {
                t.push('\n');
                t.push_str(line);
            }
            continue;
        }
        // `names : type`, or `names :` with the type on the following
        // indented lines (a long type).
        let declared = if line.starts_with(' ') {
            None
        } else {
            line.split_once(" : ").or_else(|| line.strip_suffix(" :").map(|names| (names, "")))
        };
        match declared {
            Some((names, ty)) => {
                out.hyps.push((names.split(' ').map(str::to_owned).collect(), ty.to_owned()));
                cur = Cur::Hyp;
            }
            None => {
                if let (Cur::Hyp, Some((_, ty))) = (&cur, out.hyps.last_mut()) {
                    ty.push('\n');
                    ty.push_str(line);
                }
            }
        }
    }
    out
}

/// The frozen statement's binders and target: the text after `theorem <thm>`
/// up to the final `:=`, split at the first colon outside brackets.
pub fn parse_statement(statement: &str, thm: &str) -> Option<(String, String)> {
    let rest = statement.strip_prefix("theorem")?.trim_start().strip_prefix(thm)?;
    if rest.chars().next().is_some_and(ident_cont) {
        return None;
    }
    let rest = rest.strip_suffix(":=").unwrap_or(rest);
    let mut depth = 0i64;
    for (i, c) in rest.char_indices() {
        match c {
            '(' | '[' | '{' | '⟨' => depth += 1,
            ')' | ']' | '}' | '⟩' => depth -= 1,
            ':' if depth == 0 && !rest.get(i..).is_some_and(|t| t.starts_with(":=")) => {
                return Some((rest.get(..i)?.trim().to_owned(), rest.get(i + 1..)?.trim().to_owned()));
            }
            _ => {}
        }
    }
    None
}

fn ident_start(c: char) -> bool {
    c.is_ascii_alphabetic()
        || c == '_'
        || matches!(c,
            '\u{00C0}'..='\u{024F}' | '\u{0370}'..='\u{03FF}' | '\u{1D00}'..='\u{1DBF}'
            | '\u{2100}'..='\u{214F}' | '\u{2200}'..='\u{22FF}')
}

fn ident_cont(c: char) -> bool {
    c.is_alphanumeric()
        || matches!(c, '_' | '\'' | '!' | '?' | '¹' | '²' | '³' | '✝')
        || matches!(c, '\u{2070}'..='\u{2079}' | '\u{2080}'..='\u{209C}' | '\u{0370}'..='\u{03FF}')
}

/// The identifier-like tokens of `text`.
pub fn idents(text: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if !ident_start(c) {
            continue;
        }
        let mut tok = String::from(c);
        while let Some(&n) = chars.peek() {
            if !ident_cont(n) {
                break;
            }
            tok.push(n);
            chars.next();
        }
        out.insert(tok);
    }
    out
}

fn step_names(pattern: &str) -> Vec<String> {
    idents(pattern).into_iter().filter(|n| n != "_").collect()
}

/// `set_option maxHeartbeats N in (tac)`.
fn capped(tactic: &str) -> String {
    format!("set_option maxHeartbeats {ANALYSIS_HEARTBEATS} in ({tactic})")
}

/// The `Try this: …` suggestion of a tactic's messages (a leading `[kind]`
/// tag dropped), if any.
pub fn parse_try_this(messages: &[Message]) -> Option<String> {
    let data = messages.iter().find_map(|m| m.data.split_once("Try this:").map(|(_, rest)| rest.trim()))?;
    let rest = match data.strip_prefix('[') {
        Some(tagged) => tagged.split_once(']').map_or(data, |(_, r)| r.trim_start()),
        None => data,
    };
    Some(rest.to_owned())
}

fn has_error(r: &TacticReply) -> bool {
    r.messages.iter().any(|m| m.severity == "error")
}

/// The proof state a successful tactic reply produced.
fn advanced(r: &TacticReply) -> Option<u64> {
    if r.error.is_some() || has_error(r) { None } else { r.proof_state }
}

/// A suite tactic that closed a goal, with `exact?`'s suggestion when it
/// gave one.
struct Closed {
    tactic: String,
    suggestion: Option<String>,
}

/// Run `suite` in order on `proof_state`, each as `prefix` + capped tactic;
/// a tactic closes when the reply has no error and exactly `remaining`
/// goals. The first closer, if any.
fn run_suite(
    driver: &mut Driver,
    proof_state: u64,
    prefix: &str,
    remaining: usize,
    suite: &[&str],
) -> Result<Option<Closed>, Abort> {
    for tactic in suite {
        let r = driver.tactic(&format!("{prefix}{}", capped(tactic)), proof_state)?;
        if advanced(&r).is_some() && r.goals.len() == remaining {
            return Ok(Some(Closed {
                tactic: (*tactic).to_owned(),
                suggestion: parse_try_this(&r.messages),
            }));
        }
    }
    Ok(None)
}

// ---- The frozen problem, prepared once per REPL ----

/// Most roots one REPL keeps before it starts over.
const MAX_ROOTS: usize = 64;

/// What the analysis of every candidate of one frozen file shares: the
/// environments after the frozen file, the root goal as printed, the
/// verbatim frozen target and the renamed statement head for the text route.
/// Its environment ids die with the REPL, so the owner drops it on respawn.
#[derive(Clone, Debug)]
pub(crate) struct Root {
    /// The frozen file's environment plus [`PP_OPTIONS`]: where the text
    /// route elaborates its copies.
    pp_env: u64,
    /// The root goal's locals (`name → type` as printed).
    locals: BTreeMap<String, String>,
    /// The root target as printed under [`PP_OPTIONS`].
    target_pp: String,
    /// The frozen target's source text (after the statement's top-level colon).
    target_src: String,
    /// `theorem <thm>_textroute <binders> : <target> ` (the statement without
    /// its `:=`, renamed).
    head: String,
    /// `<thm> : <type>` under `pp.all` (head as in `starts_with_type_head`), from the frozen file.
    pristine_type: Option<String>,
}

fn check_line(thm: &str) -> String {
    format!("set_option pp.all true in #check @{thm}")
}

/// The `#check` reply's type line, from an `info` message.
fn checked_type(reply: &CommandReply, thm: &str) -> Option<String> {
    reply
        .messages
        .iter()
        .find(|m| m.severity == "info" && crate::starts_with_type_head(m.data.trim_start(), thm))
        .map(|m| m.data.trim().to_owned())
}

fn renamed(statement: &str, thm: &str, suffix: &str) -> String {
    statement.replacen(&format!("theorem {thm}"), &format!("theorem {thm}_{suffix}"), 1)
}

/// Elaborate the frozen file and read the root goal. `frozen_body` is the
/// frozen file with header imports blanked; `statement` its
/// `theorem … :=` (see `extract_statement`).
fn root(
    driver: &mut Driver,
    mathlib_env: u64,
    frozen_body: &str,
    statement: &str,
    thm: &str,
) -> Result<Root, Abort> {
    let frozen = driver.command(frozen_body, mathlib_env, false)?;
    if frozen.has_errors() {
        return Err(Abort::Failed("the frozen file does not elaborate on the REPL".to_owned()));
    }
    let pristine_type = checked_type(&driver.command(&check_line(thm), frozen.env, false)?, thm);
    let pp_env = driver.command(PP_OPTIONS, frozen.env, false)?.env;
    let (_, target_src) = parse_statement(statement, thm)
        .ok_or_else(|| Abort::Failed("the frozen statement has no top-level colon".to_owned()))?;
    let head = statement.strip_suffix(":=").unwrap_or(statement);
    let copy = driver.command(&format!("{}:= by\n  sorry", renamed(head, thm, "rootcopy")), pp_env, false)?;
    let goal = copy
        .sorries
        .first()
        .map(|s| parse_goal(&s.goal))
        .ok_or_else(|| Abort::Failed("the root copy of the frozen theorem has no sorry".to_owned()))?;
    let mut locals = BTreeMap::new();
    for (names, ty) in &goal.hyps {
        for n in names {
            locals.insert(n.clone(), ty.clone());
        }
    }
    Ok(Root {
        pp_env,
        locals,
        target_pp: goal.target.unwrap_or_default(),
        target_src,
        head: renamed(head, thm, "textroute"),
        pristine_type,
    })
}

// ---- The analysis ----

/// One `analyze` request's inputs.
pub(crate) struct Input<'a> {
    /// The theorem name (the problem id).
    pub thm: &'a str,
    /// The frozen file, header imports blanked. Its [`Root`] is cached under
    /// this text, so a new statement under the same id gets its own.
    pub frozen_body: &'a str,
    /// The frozen `theorem … :=`.
    pub statement: &'a str,
    /// The candidate, header imports blanked (`prepare_check`'s output).
    pub body: &'a str,
    pub mathlib_env: u64,
}

/// A closed-step candidate before the maximality filter.
struct Cand {
    kind: StepKind,
    name: Option<String>,
    statement: Option<String>,
    by_mode: bool,
    span: (Pos, Pos),
    /// The state the tactic ran on (from `allTactics`); none for a bullet.
    proof_state: Option<u64>,
    closed: bool,
}

/// Scratch the finalisation needs beside the reply.
#[derive(Default)]
struct Work {
    /// Per step of `out.steps` (same order): its statement state was obtained.
    state_ok: Vec<bool>,
    /// Per step: the bound names (`_` dropped); empty for a bullet.
    names: Vec<Vec<String>>,
    /// Every hypothesis name of every leaf's context.
    leaf_context: BTreeSet<String>,
    restates_root: bool,
}

/// Analyse one candidate. Never fails: whatever stopped the analysis is in
/// the reply's `error`, and the counts are over what was obtained.
pub(crate) fn analyze(
    driver: &mut Driver,
    roots: &mut HashMap<String, Root>,
    input: &Input,
) -> AnalyzeResponse {
    let mut out = AnalyzeResponse {
        status: CheckStatus::Sorries,
        leaves: vec![],
        steps: vec![],
        counts: SketchCounts::default(),
        hollow: false,
        false_leaf: false,
        restates_root: false,
        statement_type_identical: None,
        truncated: false,
        suite: ANALYSIS_SUITE_ID.to_owned(),
        error: None,
        elapsed_ms: 0,
    };
    let mut work = Work::default();
    if let Err(abort) = run(driver, roots, input, &mut out, &mut work) {
        out.error = Some(abort.message());
    }
    finalize(&mut out, &work);
    out.elapsed_ms = millis(driver.elapsed());
    out
}

fn run(
    driver: &mut Driver,
    roots: &mut HashMap<String, Root>,
    input: &Input,
    out: &mut AnalyzeResponse,
    work: &mut Work,
) -> Result<(), Abort> {
    // 1. The real check.
    let real = driver.command(input.body, input.mathlib_env, false)?;
    out.status = status_of(&real);
    if out.status != CheckStatus::Sorries {
        return Ok(());
    }
    out.counts.open = real.sorries.len() as u64;
    let env1 = real.env;
    if !roots.contains_key(input.frozen_body) {
        let prepared = root(driver, input.mathlib_env, input.frozen_body, input.statement, input.thm)?;
        // Bounded like the pristine types, since callers choose the texts.
        if roots.len() >= MAX_ROOTS {
            roots.clear();
        }
        roots.insert(input.frozen_body.to_owned(), prepared);
    }
    let Some(root) = roots.get(input.frozen_body) else {
        return Err(Abort::Failed("the frozen problem could not be prepared".to_owned()));
    };
    // Statement type identity, `verify`'s check applied to a partial proof.
    let candidate_type = checked_type(&driver.command(&check_line(input.thm), env1, false)?, input.thm);
    out.statement_type_identical = match (&candidate_type, &root.pristine_type) {
        (Some(c), Some(p)) => Some(c == p),
        _ => None,
    };
    // 2. The renamed copy, in the env after the real check, with every tactic.
    let Some(start) = input.body.find(input.statement) else {
        return Err(Abort::Failed("the statement is not in the prepared candidate".to_owned()));
    };
    let pre = input.body.get(..start).unwrap_or_default();
    let chunk = input.body.get(start..).unwrap_or_default();
    let padding: String = pre.chars().map(|c| if c == '\n' { '\n' } else { ' ' }).collect();
    let copy = format!("{padding}{}", renamed(chunk, input.thm, "sketch"));
    let pp_env = driver.command(PP_OPTIONS, env1, false)?.env;
    let sketch = driver.command(&copy, pp_env, true)?;
    // The copy starts at the statement, so a `sorry` in a declaration the
    // candidate wrote *before* the theorem (a helper `def`, an auxiliary
    // lemma) is not in it. Those are open work — `counts.open` has them —
    // but they are not leaves of this theorem, so the copy is compared
    // against the theorem's own sorries only.
    let statement_line = pre.matches('\n').count() as u64 + 1;
    let in_theorem = real.sorries.iter().filter(|s| s.line >= statement_line).count();
    if sketch.has_errors() || sketch.sorries.len() != in_theorem {
        let errors: Vec<String> = sketch
            .messages
            .iter()
            .filter(|m| m.severity == "error")
            .map(|m| excerpt(&m.data))
            .take(2)
            .collect();
        return Err(Abort::Failed(format!(
            "the renamed copy differs from the candidate: {} sorries against {}; errors: {}",
            sketch.sorries.len(),
            in_theorem,
            errors.join("; ")
        )));
    }
    for s in &sketch.sorries {
        work.leaf_context.extend(parse_goal(&s.goal).names().cloned());
    }
    // 3. Closed steps.
    let sorry_positions: Vec<Pos> = sketch.sorries.iter().map(|s| Pos::of(s.line, s.column)).collect();
    let mut cands: Vec<Cand> = sketch
        .tactics
        .iter()
        .filter_map(|t| {
            let step = parse_step(&t.tactic)?;
            let span = (Pos::of(t.line, t.column), Pos::of(t.end_line, t.end_column));
            Some(Cand {
                kind: step.kind,
                name: Some(step.name),
                statement: Some(step.statement),
                by_mode: step.by_mode,
                span,
                proof_state: t.proof_state,
                closed: !sorry_positions.iter().any(|p| inside(*p, span)),
            })
        })
        .collect();
    cands.extend(bullets(&copy).into_iter().map(|span| Cand {
        kind: StepKind::Bullet,
        name: None,
        statement: None,
        by_mode: true,
        span,
        proof_state: None,
        closed: !sorry_positions.iter().any(|p| inside(*p, span)),
    }));
    let maximal: Vec<bool> = cands
        .iter()
        .map(|c| {
            c.closed
                && !cands
                    .iter()
                    .any(|o| o.closed && o.span != c.span && inside(c.span.0, o.span) && c.span.1 <= o.span.1)
        })
        .collect();
    let mut have_steps: Vec<(usize, (Pos, Pos))> = Vec::new();
    for (cand, _) in cands.iter().zip(maximal).filter(|(_, m)| *m) {
        let started = driver.elapsed();
        let (state, note) = statement_state(driver, cand, &sketch.tactics)?;
        let mut trivial_by = None;
        let mut library_trivial_by = None;
        if let Some((ps, prefix, remaining)) = state {
            trivial_by = run_suite(driver, ps, prefix, remaining, &ANALYSIS_SUITE)?.map(|c| c.tactic);
            if trivial_by.is_none() {
                // The copy is elaborated after the candidate itself, so the
                // theorem under analysis is in scope and `exact?` can close a
                // step by citing it. Such a hit is circular, not a library
                // fact, so it does not make the step trivial.
                library_trivial_by = run_suite(driver, ps, prefix, remaining, &[LIBRARY_SEARCH])?
                    .map(|c| bounded(&c.suggestion.unwrap_or(c.tactic), MAX_ANALYSIS_TEXT_CHARS))
                    .filter(|found| !found.contains(input.thm));
            }
        }
        let index = out.steps.len();
        if cand.kind != StepKind::Bullet {
            have_steps.push((index, cand.span));
        }
        work.state_ok.push(state.is_some());
        work.names.push(cand.name.as_deref().map(step_names).unwrap_or_default());
        out.steps.push(Step {
            kind: cand.kind,
            line: cand.span.0.line,
            column: cand.span.0.column,
            name: cand.name.clone(),
            statement: cand.statement.as_deref().map(|s| bounded(s, MAX_ANALYSIS_TEXT_CHARS)),
            trivial_by,
            library_trivial_by,
            unused: false,
            in_leaf_context: false,
            counted: false,
            note,
            elapsed_ms: millis(driver.elapsed().saturating_sub(started)),
        });
    }
    // 3b. Used: the linter on a sorry-free copy.
    if !have_steps.is_empty()
        && let Some(note) = lint(driver, env1, &copy, &sketch.sorries, &have_steps, out, work)?
        && out.error.is_none()
    {
        out.error = Some(note);
    }
    // 4. Leaves.
    for s in &sketch.sorries {
        let leaf = leaf(driver, root, s, work)?;
        out.leaves.push(leaf);
    }
    Ok(())
}

/// A step's statement state — where the suite runs — with the goal-selecting
/// prefix and the number of goals that remain when the statement is closed.
type StatementState = (u64, &'static str, usize);

/// The statement state of a step: a `by` block's or bullet's first inner
/// tactic state, or for a term-mode step `have this : T := ?hole` in the
/// step's own state. The note says why there is none.
fn statement_state(
    driver: &mut Driver,
    cand: &Cand,
    tactics: &[TacticInfo],
) -> Result<(Option<StatementState>, Option<String>), Abort> {
    if cand.kind == StepKind::Bullet || cand.by_mode {
        let inner = tactics
            .iter()
            .filter(|t| {
                cand.span.0 < Pos::of(t.line, t.column) && Pos::of(t.end_line, t.end_column) <= cand.span.1
            })
            .min_by_key(|t| Pos::of(t.line, t.column));
        return Ok(match inner.and_then(|t| t.proof_state.map(|ps| (ps, &t.goals))) {
            Some((ps, goals)) => {
                let remaining = if cand.kind == StepKind::Bullet {
                    let n = goals.matches("\n⊢").count();
                    n.saturating_sub(1)
                } else {
                    0
                };
                (Some((ps, "", remaining)), None)
            }
            None => (None, Some("no inner tactic".to_owned())),
        });
    }
    let (Some(ps), Some(statement)) = (cand.proof_state, &cand.statement) else {
        return Ok((None, Some("no proof state".to_owned())));
    };
    let r = driver.tactic(&format!("have this : {statement} := ?hole"), ps)?;
    Ok(match advanced(&r) {
        Some(hole) if r.goals.len() == 2 => (Some((hole, "case hole => ", 1)), None),
        _ => {
            let why = r.error.clone().unwrap_or_else(|| {
                r.messages
                    .iter()
                    .filter(|m| m.severity == "error")
                    .map(|m| m.data.clone())
                    .collect::<Vec<_>>()
                    .join("; ")
            });
            (None, Some(format!("could not restate: {}", excerpt(&why))))
        }
    })
}

/// Mark unused `have`/`obtain` steps: every `sorry` of the copy becomes the
/// axiom `leaf_hole`, the copy is linted, and steps whose every bound name is
/// reported become `skip` for another round. Returns a note when the lint
/// copy was not clean (the verdicts then stand as obtained).
fn lint(
    driver: &mut Driver,
    env1: u64,
    copy: &str,
    sorries: &[Sorry],
    have_steps: &[(usize, (Pos, Pos))],
    out: &mut AnalyzeResponse,
    work: &Work,
) -> Result<Option<String>, Abort> {
    let lint_env = driver.command(LINT_ENV, env1, false)?.env;
    let mut edited = copy.to_owned();
    let mut ordered: Vec<&Sorry> = sorries.iter().collect();
    ordered.sort_by_key(|s| std::cmp::Reverse(Pos::of(s.line, s.column)));
    for s in ordered {
        let pos = Pos::of(s.line, s.column);
        let (a, b) = (byte_offset(&edited, pos), byte_offset(&edited, Pos::of(s.end_line, s.end_column)));
        if a < b && b <= edited.len() {
            let hole = if sorry_is_tactic(copy, pos) { "exact leaf_hole" } else { "leaf_hole" };
            edited.replace_range(a..b, hole);
        }
    }
    for round in 0..LINT_ROUNDS {
        let rr = driver.command(&edited, lint_env, false)?;
        if rr.has_errors() || !rr.sorries.is_empty() {
            let errors: Vec<String> = rr
                .messages
                .iter()
                .filter(|m| m.severity == "error")
                .map(|m| excerpt(&m.data))
                .take(2)
                .collect();
            return Ok(Some(format!(
                "the lint copy is not clean (round {round}): {} sorries; {}",
                rr.sorries.len(),
                errors.join("; ")
            )));
        }
        let unused: BTreeSet<(u64, String)> = rr
            .messages
            .iter()
            .filter(|m| m.severity == "warning")
            .filter_map(|m| {
                let name = m.data.strip_prefix("unused variable `")?.split('`').next()?;
                Some((m.line, name.to_owned()))
            })
            .collect();
        let mut newly: Vec<(Pos, Pos)> = Vec::new();
        for (index, span) in have_steps {
            let Some(step) = out.steps.get_mut(*index) else { continue };
            if step.unused || step.name.as_deref() == Some("this") {
                continue;
            }
            let names = work.names.get(*index).map(Vec::as_slice).unwrap_or_default();
            if !names.is_empty() && names.iter().all(|n| unused.contains(&(span.0.line, n.clone()))) {
                step.unused = true;
                newly.push(*span);
            }
        }
        if newly.is_empty() {
            break;
        }
        newly.sort_by_key(|span| std::cmp::Reverse(span.0));
        for span in newly {
            let (a, b) = (byte_offset(&edited, span.0), byte_offset(&edited, span.1));
            if a < b && b <= edited.len() {
                let newlines = edited.get(a..b).map_or(0, |t| t.matches('\n').count());
                edited.replace_range(a..b, &format!("skip{}", "\n".repeat(newlines)));
            }
        }
    }
    Ok(None)
}

/// The hollow test of one leaf (step 4 of the module documentation), with
/// the text route as the fallback.
fn leaf(driver: &mut Driver, root: &Root, s: &Sorry, work: &mut Work) -> Result<Leaf, Abort> {
    let started = driver.elapsed();
    let goal = parse_goal(&s.goal);
    let target = goal.target.clone().unwrap_or_default();
    if target.trim() == root.target_pp.trim() || target.trim() == root.target_src.trim() {
        work.restates_root = true;
    }
    let mut leaf = Leaf {
        line: s.line,
        column: s.column,
        target: bounded(&target, MAX_ANALYSIS_TEXT_CHARS),
        verdict: LeafVerdict::Untestable,
        hollow_by: None,
        false_leaf: matches!(target.trim(), "False" | "⊥"),
        route: None,
        root_modified: false,
        note: None,
        elapsed_ms: 0,
    };
    let mut hyps = goal.hyps.clone();
    let mut ps = s.proof_state;
    // Inaccessible names get fresh ones so they can be cleared or reverted.
    let inaccessible: Vec<String> = goal.names().filter(|n| n.contains('✝')).cloned().collect();
    if !inaccessible.is_empty() {
        let fresh: Vec<String> = (0..inaccessible.len()).map(|k| format!("inacc_{k}")).collect();
        let r = driver.tactic(&format!("rename_i {}", fresh.join(" ")), ps)?;
        if let Some(renamed) = advanced(&r) {
            ps = renamed;
            let map: BTreeMap<&String, &String> = inaccessible.iter().zip(fresh.iter()).collect();
            for (names, _) in &mut hyps {
                for n in names.iter_mut() {
                    if let Some(f) = map.get(n) {
                        *n = (*f).clone();
                    }
                }
            }
        }
    }
    let names_flat: BTreeSet<&String> = hyps.iter().flat_map(|(names, _)| names.iter()).collect();
    leaf.root_modified = root.locals.keys().any(|n| !names_flat.contains(n))
        || hyps.iter().any(|(names, ty)| {
            names.iter().any(|n| root.locals.get(n).is_some_and(|rt| rt.trim() != ty.trim()))
        });
    let nonroot: Vec<String> = hyps
        .iter()
        .flat_map(|(names, _)| names.iter())
        .filter(|n| !root.locals.contains_key(*n))
        .cloned()
        .collect();
    // strip_dep: clear every sketch-introduced local newest first; what
    // `clear` refuses (a dependency of the target or of a kept local) is
    // reverted into the statement.
    let mut cleared: BTreeSet<&String> = BTreeSet::new();
    for n in nonroot.iter().rev() {
        let r = driver.tactic(&format!("clear {n}"), ps)?;
        if let Some(next) = advanced(&r) {
            ps = next;
            cleared.insert(n);
        }
    }
    let to_revert: Vec<&str> = nonroot.iter().filter(|n| !cleared.contains(n)).map(String::as_str).collect();
    if !to_revert.is_empty() {
        let r = driver.tactic(&format!("revert {}", to_revert.join(" ")), ps)?;
        match advanced(&r) {
            Some(next) => ps = next,
            None => {
                leaf.note =
                    Some(format!("revert failed: {}", excerpt(r.error.as_deref().unwrap_or_default())));
                leaf.elapsed_ms = millis(driver.elapsed().saturating_sub(started));
                return Ok(leaf);
            }
        }
    }
    let refine = format!("refine Iff.mp (a := ({})) ⟨?d1, fun H => ?hollow⟩ ?d2", root.target_src);
    let r = driver.tactic(&refine, ps)?;
    match advanced(&r) {
        Some(opened) if r.goals.len() == 3 => {
            let decided = hollowness(driver, opened, "case hollow => ", 2)?;
            leaf.hollow_by = decided.hollow_by;
            leaf.verdict = decided.verdict;
            leaf.note = decided.vacuous;
            leaf.route = Some(LeafRoute::Tactic);
        }
        _ => {
            let why = format!(
                "root target does not elaborate in the leaf's context: {}",
                excerpt(r.error.as_deref().unwrap_or_default())
            );
            // A goal that is not a proposition has no hollow test at all.
            let probe = driver.tactic("by_contra leaf_probe", ps)?;
            if advanced(&probe).is_none() {
                leaf.note = Some(format!("data-typed leaf (the target is not a proposition); {why}"));
            } else {
                text_route(driver, root, s, &mut leaf, why)?;
            }
        }
    }
    leaf.elapsed_ms = millis(driver.elapsed().saturating_sub(started));
    Ok(leaf)
}

/// What one opened hollow test decided.
struct Hollowness {
    verdict: LeafVerdict,
    /// The suite tactic that derived the theorem, when that means something.
    hollow_by: Option<String>,
    /// Why the test decided nothing, when it decided nothing.
    vacuous: Option<String>,
}

/// The control goal: `0 = 1` in place of the frozen target, reached from it
/// by `exfalso`. A hypothesis that proves this proves anything.
const VACUITY_CONTROL: &str = "exfalso; apply Nat.zero_ne_one; ";

/// Run the suite on an opened `H : <leaf> ⊢ <frozen target>`, then keep it
/// honest with a control.
///
/// A leaf the suite can *refute* — or merely reduce to nonsense — implies
/// everything, the frozen target included, so closing the goal above says
/// nothing about it. That is not a corner case. `by_contra` leaves the goal
/// `False`. And clearing a sketch's own hypotheses can leave a false
/// statement behind: the leaf `2023 * n = 0` under
/// `h : (digits 2 (2023 * n)).sum = 0` strips to `∀ n, 2023 * n = 0`, which
/// `simp_all` turns into the rewrite `∀ n, n = 0` and then closes *any*
/// numeric goal with. Without a control both would count as restatements,
/// and every proof by contradiction would score no more than the base.
///
/// So the closure is repeated against [`VACUITY_CONTROL`], a goal no
/// consistent hypothesis proves. If the suite closes that too, the verdict
/// is `Untestable` — the test ran and decided nothing — and the leaf stays
/// open work. `False` alone is too weak a control: the rewrite above
/// derives it from nothing, while `0 = 1` it rewrites to `0 = 0`.
///
/// Clearing rather than reverting is deliberate (it denies the dodge "add a
/// hypothesis the suite cannot discharge"), and this control is what it
/// costs: one extra suite run per candidate-hollow leaf, none for the rest.
fn hollowness(driver: &mut Driver, opened: u64, prefix: &str, remaining: usize) -> Result<Hollowness, Abort> {
    let Some(closed) = run_suite(driver, opened, prefix, remaining, &ANALYSIS_SUITE)? else {
        return Ok(Hollowness { verdict: LeafVerdict::NotHollow, hollow_by: None, vacuous: None });
    };
    let control = format!("{prefix}{VACUITY_CONTROL}");
    if let Some(anything) = run_suite(driver, opened, &control, remaining, &ANALYSIS_SUITE)? {
        return Ok(Hollowness {
            verdict: LeafVerdict::Untestable,
            hollow_by: None,
            vacuous: Some(format!(
                "the stripped statement proves anything (`{}` closes `0 = 1` from it), so deriving \
                 the theorem from it (`{}`) is no evidence of hollowness",
                anything.tactic, closed.tactic
            )),
        });
    }
    Ok(Hollowness { verdict: LeafVerdict::Hollow, hollow_by: Some(closed.tactic), vacuous: None })
}

/// The text route: revert every accessible local of the leaf's own state,
/// re-elaborate the printed closed statement as `have H : … := sorry` inside
/// a renamed copy of the frozen theorem (beside it, so its auxiliaries are
/// shared), and run the suite on `H : … ⊢ <frozen target>`.
fn text_route(
    driver: &mut Driver,
    root: &Root,
    s: &Sorry,
    leaf: &mut Leaf,
    why: String,
) -> Result<(), Abort> {
    let goal = parse_goal(&s.goal);
    let names: Vec<&str> = goal.names().filter(|n| !n.contains('✝')).map(String::as_str).collect();
    let stated = if names.is_empty() {
        s.goal.clone()
    } else {
        let r = driver.tactic(&format!("revert {}", names.join(" ")), s.proof_state)?;
        match (advanced(&r), r.goals.first()) {
            (Some(_), Some(g)) => g.clone(),
            _ => {
                leaf.note = Some(format!("{why} | text route: revert failed"));
                return Ok(());
            }
        }
    };
    let closed = parse_goal(&stated);
    if !closed.hyps.is_empty() {
        leaf.note = Some(format!("{why} | text route: inaccessible locals remain"));
        return Ok(());
    }
    let statement = closed.target.unwrap_or_default().replace('\n', "\n    ");
    let src = format!("{}:= by\n  have H : {statement} := sorry\n  sorry", root.head);
    let rr = driver.command(&src, root.pp_env, false)?;
    if rr.has_errors() || rr.sorries.len() < 2 {
        let errors: Vec<String> =
            rr.messages.iter().filter(|m| m.severity == "error").map(|m| excerpt(&m.data)).take(2).collect();
        leaf.note =
            Some(format!("{why} | text route: statement does not re-elaborate: {}", errors.join("; ")));
        return Ok(());
    }
    let Some(with_h) = rr.sorries.iter().find(|s| s.goal.contains("\nH :") || s.goal.starts_with("H :"))
    else {
        leaf.note = Some(format!("{why} | text route: no state with H"));
        return Ok(());
    };
    let decided = hollowness(driver, with_h.proof_state, "", 0)?;
    leaf.hollow_by = decided.hollow_by;
    leaf.verdict = decided.verdict;
    leaf.route = Some(LeafRoute::Text);
    leaf.note = Some(match decided.vacuous {
        Some(vacuous) => format!("{why} | {vacuous}"),
        None => why,
    });
    Ok(())
}

/// Counts, flags, ordering and bounds — over whatever was obtained.
fn finalize(out: &mut AnalyzeResponse, work: &Work) {
    let mut counts = SketchCounts { open: out.counts.open, ..SketchCounts::default() };
    for (i, step) in out.steps.iter_mut().enumerate() {
        let state_ok = work.state_ok.get(i).copied().unwrap_or(false);
        let names = work.names.get(i).map(Vec::as_slice).unwrap_or_default();
        step.in_leaf_context = step.kind == StepKind::Bullet
            || step.name.as_deref() == Some("this")
            || names.iter().any(|n| work.leaf_context.contains(n));
        let non_trivial = state_ok && step.trivial_by.is_none() && step.library_trivial_by.is_none();
        step.counted = non_trivial && !step.unused;
        if step.counted {
            counts.closed_with_bullets += 1;
            if step.kind != StepKind::Bullet {
                counts.closed += 1;
            }
        }
        if step.kind != StepKind::Bullet && non_trivial && (!step.unused || step.in_leaf_context) {
            counts.closed_leaf_used += 1;
        }
    }
    counts.untestable = out.leaves.iter().filter(|l| l.verdict == LeafVerdict::Untestable).count() as u64;
    out.counts = counts;
    out.hollow = out.leaves.iter().any(|l| l.verdict == LeafVerdict::Hollow);
    out.false_leaf = out.leaves.iter().any(|l| l.false_leaf);
    out.restates_root = work.restates_root;
    out.steps.sort_by_key(|s| (s.line, s.column));
    out.leaves.sort_by_key(|l| (l.line, l.column));
    if out.steps.len() > MAX_ANALYSIS_ITEMS || out.leaves.len() > MAX_ANALYSIS_ITEMS {
        out.truncated = true;
        out.steps.truncate(MAX_ANALYSIS_ITEMS);
        out.leaves.truncate(MAX_ANALYSIS_ITEMS);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn steps_goals_and_statements_are_parsed_from_text() {
        let term = parse_step("have hqb' : e q ∈ f b := ⟨q, hqb, rfl⟩").unwrap();
        assert_eq!(
            (term.kind, term.name.as_str(), term.statement.as_str(), term.by_mode),
            (StepKind::Have, "hqb'", "e q ∈ f b", false)
        );
        let block =
            parse_step("have hne : Nonempty (ℚ ≃ ℕ+) := by\n    apply nonempty_equiv_of_countable").unwrap();
        assert!(block.by_mode && block.statement == "Nonempty (ℚ ≃ ℕ+)");
        let anon = parse_step("have : ∀ x : ℕ, x = x := fun _ => rfl").unwrap();
        assert_eq!((anon.name.as_str(), anon.statement.as_str()), ("this", "∀ x : ℕ, x = x"));
        let pat = parse_step("obtain ⟨m, hm⟩ : ∃ m, m ≤ n ∧ n ≤ m := ⟨n, le_rfl, le_rfl⟩").unwrap();
        assert_eq!((pat.kind, pat.name.as_str()), (StepKind::Obtain, "⟨m, hm⟩"));
        assert_eq!(step_names(&pat.name), ["hm", "m"]);
        assert_eq!(parse_step("obtain ⟨q, hq⟩ := h"), None, "no stated type, nothing to test");
        assert_eq!(parse_step("haveI : Fact p := ⟨hp⟩"), None);
        assert_eq!(parse_step("exact h"), None);

        let goal = parse_goal(
            "case right\nhne : Nonempty (ℚ ≃ ℕ+)\ne : ℚ ≃ ℕ+ := Classical.choice hne\na b : ℝ\nh :\n  ∀ (x : ℝ),\n    x = x\n⊢ e q ∉ f a",
        );
        assert_eq!(goal.hyps[0].0, ["hne"], "the case line is not a hypothesis");
        assert_eq!(goal.hyps[1], (vec!["e".to_owned()], "ℚ ≃ ℕ+ := Classical.choice hne".to_owned()));
        assert_eq!(goal.hyps[2].0, ["a", "b"]);
        assert_eq!(goal.hyps[3].1, "\n  ∀ (x : ℝ),\n    x = x");
        assert_eq!(goal.target.as_deref(), Some("e q ∉ f a"));

        let statement = "theorem putnam_1962_b2\n: ∃ f : ℝ → Set ℕ+, ∀ a b : ℝ, a < b → f a ⊂ f b :=";
        assert_eq!(
            parse_statement(statement, "putnam_1962_b2"),
            Some((String::new(), "∃ f : ℝ → Set ℕ+, ∀ a b : ℝ, a < b → f a ⊂ f b".to_owned()))
        );
        assert_eq!(
            parse_statement("theorem t (n : ℕ) (h : 0 < n) : n ≠ 0 :=", "t"),
            Some(("(n : ℕ) (h : 0 < n)".to_owned(), "n ≠ 0".to_owned()))
        );
        assert_eq!(
            bullets("by\n  constructor\n  · intro n\n    exact n\n\n  · simp\n"),
            vec![(Pos::of(3, 2), Pos::of(4, 11)), (Pos::of(6, 2), Pos::of(6, 8)),]
        );
        assert_eq!(
            parse_try_this(&[Message {
                severity: "info".into(),
                data: "Try this:\n  [apply] exact nonempty_equiv_of_countable".into(),
                line: 0,
                column: 0
            }])
            .as_deref(),
            Some("exact nonempty_equiv_of_countable")
        );
    }

    #[test]
    fn a_sorry_is_a_tactic_or_a_term_by_its_surroundings_and_splicing_keeps_positions() {
        let text = "theorem t : P := by\n  have h : Q := by\n    sorry\n  have g : R :=\n    sorry\n  exact ⟨sorry, h⟩\n  · sorry\n  refine ?_ <;> sorry";
        assert!(sorry_is_tactic(text, Pos::of(3, 4)), "starts a tactic line");
        assert!(!sorry_is_tactic(text, Pos::of(5, 4)), "the line before ends in :=");
        assert!(!sorry_is_tactic(text, Pos::of(6, 9)), "inside an anonymous constructor");
        assert!(sorry_is_tactic(text, Pos::of(7, 4)), "after a bullet");
        assert!(sorry_is_tactic(text, Pos::of(8, 16)), "after <;>");
        // Columns are codepoints: `⟨` is one column, three bytes.
        let spliced = replace_at(text, Pos::of(6, 9), "sorry", "(by simp)").unwrap();
        assert!(spliced.contains("exact ⟨(by simp), h⟩"), "{spliced}");
        assert_eq!(spliced.lines().count(), text.lines().count());
        assert_eq!(replace_at(text, Pos::of(6, 8), "sorry", "x"), None, "nothing to replace there");
        assert_eq!(byte_offset("ab\n⟨c", Pos::of(2, 1)), 6);
    }
}
