//! The service against a real Lean, REPL and Mathlib: opt-in
//! (`#[ignore]`), run with `cargo test -p nucleus-lean -- --ignored`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use crate::verify::{Rejection, lean_runner};
use std::time::Instant;

const FROZEN: &str = "import Mathlib\n\ntheorem demo_thm (n : Nat) : n + 0 = n :=\nsorry\n";

/// `lake env <cmd>` in the PutnamBench directory, trimmed stdout.
fn lake_env(pb_lean: &str, args: &[&str]) -> String {
    let out = std::process::Command::new("lake").arg("env").args(args).current_dir(pb_lean).output().unwrap();
    assert!(out.status.success(), "lake env {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

// Real end-to-end check against a local Lean: the strict `verify`
// path cold, then `check` on a resident REPL. Opt-in: needs the PutnamBench
// environment (PB_LEAN=~/putnambench/lean4) and the built REPL binary
// (NUCLEUS_REPL=~/lean-repl/.lake/build/bin/repl), with `lake` on PATH.
// Run with `cargo test -p nucleus-lean -- --ignored`.
#[test]
#[ignore = "needs a local Lean + Mathlib environment (PB_LEAN) and the REPL binary (NUCLEUS_REPL)"]
fn end_to_end_against_real_lean() {
    let (pb, repl_exe) = (
        std::env::var("PB_LEAN").expect("PB_LEAN must name the PutnamBench lean4 directory"),
        std::env::var("NUCLEUS_REPL").expect("NUCLEUS_REPL must name the Lean REPL"),
    );
    let run = lean_runner(pb.clone().into());
    let pristine = pristine_type(FROZEN, "demo_thm", &run).unwrap();
    assert!(pristine.starts_with("demo_thm : "), "{pristine}");
    // Honest: same statement, an unrelated helper lemma first, real proof.
    let good = "import Mathlib\n\nlemma helper_unused (k : Nat) : k = k := rfl\n\ntheorem demo_thm (n : Nat) : n + 0 = n :=\n by simp\n";
    assert_eq!(
        verify_text(FROZEN, &pristine, good, "demo_thm", &run).map(|v| v.axioms),
        Ok(vec!["propext".into()])
    );
    // Bogus proof: rejected.
    let bogus = "import Mathlib\n\ntheorem demo_thm (n : Nat) : n + 0 = n :=\n by bogus_tac\n";
    assert!(matches!(
        verify_text(FROZEN, &pristine, bogus, "demo_thm", &run),
        Err(Rejection::Elaboration(_))
    ));
    // Rewritten meaning behind matching surface text: `Nat` shadowed so the
    // statement reads the same but elaborates to a different type.
    let shadow =
        "import Mathlib\n\nabbrev Nat := Bool\n\ntheorem demo_thm (n : Nat) : n + 0 = n :=\n by simp\n";
    let got = verify_text(FROZEN, &pristine, shadow, "demo_thm", &run);
    assert!(matches!(got, Err(Rejection::TypeMismatch) | Err(Rejection::Elaboration(_))), "{got:?}");

    // ---- `check` on the resident REPL, configured exactly as `run` would ----
    let scratch = std::env::temp_dir().join(format!("nucleus_lean_real_check_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    std::fs::create_dir_all(&scratch).unwrap();
    let problems = scratch.join("problems");
    std::fs::create_dir_all(&problems).unwrap();
    std::fs::write(problems.join("demo_thm.lean"), FROZEN).unwrap();
    let lean = std::fs::canonicalize(lake_env(&pb, &["which", "lean"])).unwrap();
    let options = Options {
        workspace: PathBuf::from(&pb),
        scratch: scratch.clone(),
        problems: problems.clone(),
        endpoint: EndpointName::new(ENDPOINT).unwrap(),
        timeout: Duration::from_secs(180),
        lean,
        lean_path: lake_env(&pb, &["printenv", "LEAN_PATH"]),
        repl: Some(PathBuf::from(repl_exe)),
        repl_max_requests: DEFAULT_REPL_MAX_REQUESTS,
    };
    let config = repl_config(&options).unwrap().unwrap();
    let t = Instant::now();
    let checker = ReplChecker::spawn(config).unwrap();
    eprintln!("resident REPL + import Mathlib: {:?} -> env {}", t.elapsed(), checker.mathlib_env());
    let mut service = LeanService::new(&problems, run).with_checker(checker);
    let problem = ResourcePath::new(["demo_thm"]).unwrap();
    let check = |service: &mut LeanService<_>, proof: &str| {
        let candidate = format!("import Mathlib\n\ntheorem demo_thm (n : Nat) : n + 0 = n := by\n{proof}\n");
        let t = Instant::now();
        let reply = service.check(&problem, &CheckRequest { candidate }).unwrap();
        eprintln!("check {proof:?}: {:?} -> {reply:?}", t.elapsed());
        reply
    };
    // A partial proof: the sorry comes back as the open goal, with its position.
    let partial = check(&mut service, "  have h : n = n := rfl\n  sorry");
    assert_eq!(partial.status, CheckStatus::Sorries, "{partial:?}");
    assert!(partial.errors.is_empty(), "{partial:?}");
    assert_eq!(partial.sorries.len(), 1, "{partial:?}");
    assert_eq!((partial.sorries[0].line, partial.sorries[0].column), (5, 2));
    assert!(partial.sorries[0].goal.contains("h : n = n") && partial.sorries[0].goal.contains("⊢ n + 0 = n"));
    // A broken step: Lean's error, at its position.
    let broken = check(&mut service, "  bogus_tac");
    assert_eq!(broken.status, CheckStatus::Errors, "{broken:?}");
    assert_eq!(broken.errors[0].line, 4, "{broken:?}");
    assert!(!broken.errors[0].message.is_empty());
    // A finished proof is only a candidate solve; the strict verify confirms it.
    let done = check(&mut service, "  simp");
    assert_eq!(done.status, CheckStatus::Complete, "{done:?}");
    assert!(done.elapsed_ms < 30_000, "warm check, not a cold run: {} ms", done.elapsed_ms);
    let verified = service
        .verify(
            &problem,
            &VerifyRequest {
                candidate: "import Mathlib\n\ntheorem demo_thm (n : Nat) : n + 0 = n := by\n  simp\n".into(),
            },
        )
        .unwrap();
    assert!(verified.accepted, "{verified:?}");
    // Banned even in a partial proof, decided without Lean.
    let banned = check(&mut service, "  native_decide");
    assert_eq!(banned.status, CheckStatus::Invalid);
    // `analyze` prepares each frozen file once: the same id registered with
    // another statement in another run is analysed against that statement,
    // not the first one.
    for (run, statement) in [("root-a", "n + 0 = n"), ("root-b", "0 + n = n")] {
        let frozen = format!("import Mathlib\n\ntheorem demo_root (n : Nat) : {statement} :=\nsorry\n");
        let root = ResourcePath::new([run, "demo_root"]).unwrap();
        let registered = service.register(&root, &RegisterRequest { frozen: frozen.clone() }).unwrap();
        assert!(registered.accepted, "{registered:?}");
        let reply = service.analyze(&root, &AnalyzeRequest { candidate: frozen }).unwrap();
        assert_eq!(reply.statement_type_identical, Some(true), "{statement}: {reply:?}");
    }

    assert_eq!(service.checker().unwrap().respawns(), 0, "one process served every check");
    let pid = service.checker().unwrap().pid().unwrap();
    drop(service);
    assert!(!crate::test_process_exists(pid), "the REPL died with the service");
    let _ = std::fs::remove_dir_all(&scratch);
}

/// A proof of `putnam_1962_b2` that `verify` accepts; the sketch and
/// completion cases below leave parts of it open.
const B2_VERIFIED_PROOF: &str = "  -- Use a fixed bijection between ℚ and ℕ⁺
  have hne : Nonempty (ℚ ≃ ℕ+) := by
    apply nonempty_equiv_of_countable
  let e : ℚ ≃ ℕ+ := Classical.choice hne
  -- Define f via Dedekind cuts transferred through e
  let f : \u{211d} → Set ℕ+ := fun x => { n : ℕ+ | ∃ q : ℚ, q < x ∧ e q = n }
  refine ⟨f, ?_⟩
  intro a b hab
  constructor
  · intro n hn
    rcases hn with ⟨q, hq_lt, hq_eq⟩
    exact ⟨q, lt_trans hq_lt hab, hq_eq⟩
  · intro hsub
    -- To show strictness, it suffices to find n in f b not in f a.
    -- Pick a rational q with a < q < b (exists by density of ℚ in \u{211d})
    rcases exists_rat_btwn hab with ⟨q, haq, hqb⟩
    -- q < b so e q ∈ f b
    have hqb' : e q ∈ f b := ⟨q, hqb, rfl⟩
    -- If e q ∈ f a, then there is q' < a with e q' = e q, so q' = q < a, contradiction.
    have hqa_not : e q ∉ f a := by
      intro hqa
      rcases hqa with ⟨q', hq'a, hq'eq⟩
      have hq_eq : q' = q := e.injective hq'eq
      have : (q' : \u{211d}) < a := hq'a
      rw [hq_eq] at this
      linarith
    exact hqa_not (hsub hqb')";

// The v3 procedures against the real REPL on `putnam_1962_b2`: the
// adversarial cases (root, `sorry False`, `rfl` padding), a genuine sketch
// of the verified proof, and completion of that proof with its last tactic
// re-sorried (fixture A). Opt-in like
// `end_to_end_against_real_lean`, plus the frozen statements directory
// (`PB_FROZEN`, default `$PB_LEAN/../wsolution`). Run with
// `cargo test -p nucleus-lean -- --ignored sketch --nocapture`.
#[test]
#[ignore = "needs PB_LEAN, NUCLEUS_REPL and the frozen statements (PB_FROZEN or $PB_LEAN/../wsolution)"]
fn sketch_and_completion_against_real_lean() {
    use nucleus_lean_interface::{AnalyzeRequest, CompleteRequest, LeafVerdict, SorryPosition, StepKind};
    let (pb, repl_exe) = (
        std::env::var("PB_LEAN").expect("PB_LEAN must name the PutnamBench lean4 directory"),
        std::env::var("NUCLEUS_REPL").expect("NUCLEUS_REPL must name the Lean REPL"),
    );
    let frozen_dir =
        std::env::var("PB_FROZEN").map_or_else(|_| PathBuf::from(&pb).join("../wsolution"), PathBuf::from);
    let frozen = std::fs::read_to_string(frozen_dir.join("putnam_1962_b2.lean")).unwrap();
    let scratch = std::env::temp_dir().join(format!("nucleus_lean_real_sketch_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    std::fs::create_dir_all(&scratch).unwrap();
    let lean = std::fs::canonicalize(lake_env(&pb, &["which", "lean"])).unwrap();
    let options = Options {
        workspace: PathBuf::from(&pb),
        scratch: scratch.clone(),
        problems: frozen_dir.clone(),
        endpoint: EndpointName::new(ENDPOINT).unwrap(),
        timeout: Duration::from_secs(180),
        lean,
        lean_path: lake_env(&pb, &["printenv", "LEAN_PATH"]),
        repl: Some(PathBuf::from(repl_exe)),
        repl_max_requests: DEFAULT_REPL_MAX_REQUESTS,
    };
    let config = repl_config(&options).unwrap().unwrap();
    let checker = ReplChecker::spawn(config).unwrap();
    let mut service = LeanService::new(&frozen_dir, lean_runner(pb.clone().into())).with_checker(checker);
    let problem = ResourcePath::new(["putnam_1962_b2"]).unwrap();
    let with_proof = |proof: &str| {
        let head = &frozen[..frozen.rfind(":=").unwrap() + 2];
        format!("{head} by\n{proof}\n")
    };
    let analyze = |service: &mut LeanService<_>, label: &str, candidate: String| {
        let t = Instant::now();
        let reply = service.analyze(&problem, &AnalyzeRequest { candidate }).unwrap();
        eprintln!(
            "analyze {label}: {:?} status={:?} counts={:?} hollow={} false_leaf={} restates_root={} type_identical={:?} error={:?}",
            t.elapsed(),
            reply.status,
            reply.counts,
            reply.hollow,
            reply.false_leaf,
            reply.restates_root,
            reply.statement_type_identical,
            reply.error
        );
        for step in &reply.steps {
            eprintln!(
                "  step L{} {:?} {:?} trivial={:?} library={:?} unused={} in_leaf={} counted={} note={:?}",
                step.line,
                step.kind,
                step.name,
                step.trivial_by,
                step.library_trivial_by,
                step.unused,
                step.in_leaf_context,
                step.counted,
                step.note
            );
        }
        for leaf in &reply.leaves {
            eprintln!(
                "  leaf L{} {:?} by={:?} route={:?} false={} root_modified={} target={:?} note={:?}",
                leaf.line,
                leaf.verdict,
                leaf.hollow_by,
                leaf.route,
                leaf.false_leaf,
                leaf.root_modified,
                leaf.target.chars().take(60).collect::<String>(),
                leaf.note
            );
        }
        assert_eq!(reply.error, None, "{label}: {reply:?}");
        assert_eq!(reply.status, CheckStatus::Sorries, "{label}");
        reply
    };
    // The frozen statement itself: its one leaf is the theorem, hollow by `assumption`.
    let root = analyze(&mut service, "root", frozen.clone());
    assert_eq!(root.leaves.len(), 1);
    assert_eq!(
        (root.leaves[0].verdict, root.leaves[0].hollow_by.as_deref()),
        (LeafVerdict::Hollow, Some("assumption"))
    );
    assert!(root.hollow && root.restates_root && !root.false_leaf);
    assert_eq!(root.counts.closed, 0);
    assert_eq!(root.statement_type_identical, Some(true), "the frozen file against itself");
    // `sorry False`: recorded as a False leaf, and *not* hollow — the
    // theorem follows from `False` for free, so the vacuity control fires
    // and the test decides nothing. The case is worth the
    // base score because it closes nothing.
    let false_leaf =
        analyze(&mut service, "sorry_False", with_proof("  have h : False := by\n    sorry\n  exact h.elim"));
    assert!(false_leaf.false_leaf && !false_leaf.hollow, "{false_leaf:?}");
    assert_eq!(false_leaf.leaves[0].verdict, LeafVerdict::Untestable);
    assert_eq!(false_leaf.leaves[0].hollow_by, None);
    assert!(
        false_leaf.leaves[0].note.as_deref().is_some_and(|n| n.contains("proves anything")),
        "the control's reason is recorded: {:?}",
        false_leaf.leaves[0].note
    );
    assert_eq!(false_leaf.counts.untestable, 1);
    assert_eq!(false_leaf.counts.closed, 0, "nothing counts, so it scores the base");

    // A helper declaration with a hole of its own, before the theorem: its
    // `sorry` is open work but not a leaf of this theorem, and the analysis
    // still runs.
    let with_helper = format!(
        "import Mathlib\n\nnoncomputable def helper_aux : ℝ := sorry\n{}",
        with_proof("  have h : (1 : ℝ) = 1 := rfl\n  sorry").trim_start_matches("import Mathlib\n")
    );
    let helped = analyze(&mut service, "helper_with_hole", with_helper);
    assert_eq!(helped.counts.open, 2, "both holes are open work");
    assert_eq!(helped.leaves.len(), 1, "only the theorem's hole is a leaf");
    assert!(helped.error.is_none(), "the analysis ran: {:?}", helped.error);

    // The verified proof with its last lemma left open: a genuine sketch.
    let last_lemma = B2_VERIFIED_PROOF.replace(
        "      intro hqa\n      rcases hqa with ⟨q', hq'a, hq'eq⟩\n      have hq_eq : q' = q := e.injective hq'eq\n      have : (q' : \u{211d}) < a := hq'a\n      rw [hq_eq] at this\n      linarith",
        "      sorry",
    );
    assert_ne!(last_lemma, B2_VERIFIED_PROOF);
    let sketch = analyze(&mut service, "last_lemma_sorried", with_proof(&last_lemma));
    assert!(!sketch.hollow, "{sketch:?}");
    assert_eq!(sketch.leaves[0].verdict, LeafVerdict::NotHollow);
    assert_eq!(sketch.counts.open, 1);
    assert!(
        sketch.steps.iter().any(|s| s.kind == StepKind::Bullet && s.counted),
        "the closed bullet counts with bullets"
    );
    // Ten `rfl` facts then the theorem: every step trivial, the leaf hollow.
    let padding = format!("{}\n  sorry", ["  have : 1 = 1 := rfl"; 10].join("\n"));
    let padded = analyze(&mut service, "pad_rfl", with_proof(&padding));
    assert_eq!(padded.steps.len(), 10);
    assert!(padded.steps.iter().all(|s| s.trivial_by.is_some()), "{padded:?}");
    assert_eq!(padded.counts.closed, 0);
    assert!(padded.hollow);
    // Completion, fixture A: the verified proof with its `linarith` re-sorried closes and verifies.
    let fixture_a = with_proof(&B2_VERIFIED_PROOF.replace("      linarith", "      sorry"));
    let t = Instant::now();
    let done = service.complete(&problem, &CompleteRequest { candidate: fixture_a }).unwrap();
    eprintln!(
        "complete fixture A: {:?} status={:?} all_closed={} auto_closable={} error={:?}",
        t.elapsed(),
        done.status,
        done.all_closed,
        done.auto_closable,
        done.error
    );
    for closer in &done.closers {
        eprintln!(
            "  sorry L{}:{} {:?} closer={:?} attempts={:?}",
            closer.line,
            closer.column,
            closer.position,
            closer.tactic,
            closer
                .attempts
                .iter()
                .map(|a| format!("{}:{}:{}ms", a.tactic, a.outcome, a.elapsed_ms))
                .collect::<Vec<_>>()
        );
    }
    assert_eq!(done.error, None, "{done:?}");
    assert!(done.all_closed, "{done:?}");
    assert_eq!(
        (done.closers[0].position, done.closers[0].tactic.as_deref()),
        (SorryPosition::Tactic, Some("linarith"))
    );
    let spliced = done.spliced.clone().unwrap();
    let verified = service.verify(&problem, &VerifyRequest { candidate: spliced }).unwrap();
    eprintln!("verify spliced: {verified:?}");
    assert!(verified.accepted, "{verified:?}");
    let checker = service.checker().unwrap();
    assert_eq!((checker.respawns(), checker.restarts(), checker.requests()), (0, 0, 6));
    let pid = checker.pid().unwrap();
    drop(service);
    assert!(!crate::test_process_exists(pid), "the REPL died with the service");
    let _ = std::fs::remove_dir_all(&scratch);
    // The library search is part of the triviality test, so `hne` is
    // trivial (`exact?` proves it) and of the last-lemma sketch only the
    // bullet counts.
    assert_eq!((sketch.counts.closed, sketch.counts.closed_with_bullets), (0, 1), "{:?}", sketch.counts);
}

/// Forgeries of the frozen preamble that Lean accepts with the pristine
/// `pp.all` type, which names constants but not their bodies: a `def` given
/// a new body, extended by the next line, kept only inside a doc comment
/// (`/--/` opens one), or given a new body when a comment line of the frozen
/// file starts with `theorem` (the statement is read on code, so that line
/// does not end the preamble); a `protected` field added to a `structure`, which
/// passes the textual command rule; an attributed field, which Lean itself
/// refuses; and a multi-line string literal without its blank line. The
/// separator `verify` puts after the frozen preamble turns a field line into
/// a command that does not parse, and the string is compared character for
/// character. An honest proof after a structure is still accepted.
#[test]
#[ignore = "needs a local Lean + Mathlib environment (PB_LEAN)"]
fn the_frozen_preamble_cannot_be_redefined_or_extended_against_real_lean() {
    let pb = std::env::var("PB_LEAN").expect("PB_LEAN must name the PutnamBench lean4 directory");
    let run = lean_runner(pb.into());
    let with_def = "import Mathlib\n\ndef demo_f (n : Nat) : Nat := n + 1\n\ntheorem demo_def : demo_f 0 = 2 :=\nsorry\n";
    let with_struct =
        "import Mathlib\n\nstructure Pt where\n  x : Nat\n\ntheorem demo_pt (p : Pt) : p.x = 0 :=\nsorry\n";
    let with_string =
        "import Mathlib\n\ndef s : String := \"a\n\nb\"\n\ntheorem demo_str : s.length = 3 :=\nsorry\n";
    let with_comment = "import Mathlib\n\n/-\ntheorem demo_shift is about the answer below.\n-/\n\
                        def answer : ℕ := 5\n\ntheorem demo_shift : answer = 6 :=\nsorry\n";
    let proof = |frozen: &str, from: &str, to: &str, by: &str| {
        frozen.replacen(from, to, 1).replace("\nsorry\n", &format!("\n {by}\n"))
    };
    let forged = [
        (with_def, "demo_def", proof(with_def, "n + 1\n", "2\n", "rfl")),
        (with_def, "demo_def", proof(with_def, "n + 1\n", "n + 1\n\n  * 2\n", "rfl")),
        (
            with_def,
            "demo_def",
            proof(with_def, "def demo_f", "/--/\ndef demo_f", "rfl").replacen(
                "\ntheorem",
                "\nlemma x -/ def demo_f (n : Nat) : Nat := 2\ntheorem",
                1,
            ),
        ),
        (
            with_struct,
            "demo_pt",
            proof(with_struct, "  x : Nat\n", "  x : Nat\n  protected h : x = 0\n", "p.h"),
        ),
        (
            with_struct,
            "demo_pt",
            proof(with_struct, "  x : Nat\n", "  x : Nat\n  @[simp] h (n : Nat) : x = 0\n", "p.h 0"),
        ),
        (with_string, "demo_str", proof(with_string, "\"a\n\nb\"", "\"a\nb\"", "by decide")),
        (
            with_comment,
            "demo_shift",
            // The frozen text from its comment through the statement, inside
            // a nested comment, after a `def answer` of the candidate's own.
            with_comment
                .replacen("/-\n", "def answer : ℕ := 6\n\n/- /-\n", 1)
                .replace("= 6 :=\nsorry\n", "= 6 :=\n-/\n\ntheorem demo_shift : answer = 6 :=\n rfl\n"),
        ),
    ];
    let accepted: Vec<&str> = forged
        .iter()
        .filter_map(|(frozen, thm, candidate)| {
            let pristine = pristine_type(frozen, thm, &run).unwrap();
            let got = verify_text(frozen, &pristine, candidate, thm, &run);
            eprintln!("{candidate}  -> {got:?}\n");
            got.is_ok().then_some(candidate.as_str())
        })
        .collect();
    assert!(accepted.is_empty(), "forged proofs accepted: {accepted:#?}");
    let honest = with_struct.replace("p.x = 0", "p.x + 0 = p.x");
    let candidate =
        proof(&honest, "\ntheorem", "\nlemma helper (p : Pt) : p.x = p.x := rfl\n\ntheorem", "by simp");
    let pristine = pristine_type(&honest, "demo_pt", &run).unwrap();
    let got = verify_text(&honest, &pristine, &candidate, "demo_pt", &run);
    assert!(got.is_ok(), "{candidate}: {got:?}");
}

/// `register` against real Lean, on the resource a discovery run uses: a
/// theorem declared inside a namespace is refused with Lean's own error on
/// the appended `#print axioms` and the hint; so is `@[simp]` alone before
/// the theorem, with Lean's error at verify's separator and its hint; `open
/// … in` before the theorem and a header other than `import Mathlib` are
/// refused before Lean runs; a good statement is accepted after one Lean
/// run, and a `verify` of it then runs Lean once more, on the registered
/// text.
#[test]
#[ignore = "needs a local Lean + Mathlib environment (PB_LEAN)"]
fn register_against_real_lean() {
    let pb = std::env::var("PB_LEAN").expect("PB_LEAN must name the PutnamBench lean4 directory");
    let problems = std::env::temp_dir().join(format!("nucleus_lean_real_register_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&problems);
    std::fs::create_dir_all(&problems).unwrap();
    let runs = std::cell::Cell::new(0usize);
    let lean = lean_runner(pb.into());
    let mut service = LeanService::new(&problems, |probe: &str| {
        runs.set(runs.get() + 1);
        lean(probe)
    });
    let at = |id: &str| ResourcePath::new(["real-run", id]).unwrap();
    let mut register = |id: &str, frozen: &str| {
        let t = Instant::now();
        let reply = service.register(&at(id), &RegisterRequest { frozen: frozen.to_owned() }).unwrap();
        eprintln!("register {id}: {:?} -> {reply:?}", t.elapsed());
        reply
    };
    let namespaced =
        "import Mathlib\n\nnamespace Demo\n\ntheorem demo_ns (n : Nat) : n + 0 = n :=\nsorry\n\nend Demo\n";
    let refused = register("demo_ns", namespaced);
    assert_eq!((refused.accepted, refused.stage), (false, Some(RegisterStage::Lean)));
    let output = refused.lean_output.unwrap();
    assert!(output.to_lowercase().contains("unknown constant"), "{output}");
    assert!(output.ends_with(&register_hint("demo_ns")), "{output}");
    assert_eq!(runs.get(), 1);
    // Lean runs the file as verify builds it, so a preamble whose last line
    // only begins the theorem's declaration is refused here: verify's own
    // command after it would make every proof fail to parse.
    let attributed = "import Mathlib\n\n@[simp]\ntheorem demo_attr (n : Nat) : n + 0 = n :=\nsorry\n";
    let refused = register("demo_attr", attributed);
    assert_eq!((refused.accepted, refused.stage), (false, Some(RegisterStage::Lean)), "{refused:?}");
    let output = refused.lean_output.unwrap();
    assert!(output.contains("unexpected token '#print'"), "{output}");
    assert!(output.ends_with(&separator_hint(3)), "{output}");
    assert_eq!(runs.get(), 2);
    let scoped = "import Mathlib\n\nopen Real in\ntheorem demo_in : Real.pi > 0 :=\nsorry\n";
    assert_eq!(register("demo_in", scoped).stage, Some(RegisterStage::Source));
    let tactic = "import Mathlib.Tactic\n\ntheorem demo_hdr (n : Nat) : n + 0 = n :=\nsorry\n";
    assert_eq!(register("demo_hdr", tactic).stage, Some(RegisterStage::Header));
    assert_eq!(runs.get(), 2, "refused before Lean runs");
    let good = "import Mathlib\n\ntheorem demo_reg (n : Nat) : n + 0 = n :=\nsorry\n";
    assert!(register("demo_reg", good).accepted);
    assert_eq!(runs.get(), 3);
    let task = ResourcePath::new(["real-run", "demo_reg", "t000001"]).unwrap();
    let proof = VerifyRequest { candidate: good.replace("\nsorry\n", "\n by simp\n") };
    let verified = service.verify(&task, &proof).unwrap();
    assert!(verified.accepted, "{verified:?}");
    assert_eq!(runs.get(), 4, "verify ran Lean once: the pristine type came from register");
    let _ = std::fs::remove_dir_all(&problems);
}

/// A two-operation budget against the real REPL: the third operation runs on
/// a new process that imported Mathlib again, the old one is reaped, and a
/// candidate's definitions stay invisible to later operations on both sides
/// of the boundary.
#[test]
#[ignore = "needs PB_LEAN and NUCLEUS_REPL"]
fn recycling_reimports_mathlib_and_preserves_isolation_against_real_lean() {
    let pb = std::env::var("PB_LEAN").expect("PB_LEAN must name the PutnamBench lean4 directory");
    let repl_exe = std::env::var("NUCLEUS_REPL").expect("NUCLEUS_REPL must name the Lean REPL");
    let scratch = std::env::temp_dir().join(format!("nucleus_lean_real_recycle_{}", std::process::id()));
    std::fs::create_dir_all(&scratch).unwrap();
    let lean = std::fs::canonicalize(lake_env(&pb, &["which", "lean"])).unwrap();
    let options = Options {
        workspace: PathBuf::from(&pb),
        scratch: scratch.clone(),
        problems: scratch.clone(),
        endpoint: EndpointName::new(ENDPOINT).unwrap(),
        timeout: Duration::from_secs(180),
        lean,
        lean_path: lake_env(&pb, &["printenv", "LEAN_PATH"]),
        repl: Some(PathBuf::from(repl_exe)),
        repl_max_requests: 2,
    };
    let mut checker = ReplChecker::spawn_with_max_requests(
        repl_config(&options).unwrap().unwrap(),
        options.repl_max_requests,
    )
    .unwrap();
    let first = checker.pid().unwrap();
    let (defined, _) = checker
        .check("def nucleus_recycling_probe : ℕ := 1\nexample : (1 : ℕ) + 1 = 2 := by norm_num\n")
        .unwrap();
    assert!(!defined.has_errors(), "{defined:?}");
    let (isolated, _) = checker.check("#check nucleus_recycling_probe\n").unwrap();
    assert!(isolated.has_errors(), "the prior candidate must not define names in the Mathlib base");
    assert_eq!((checker.requests(), checker.pid()), (2, Some(first)));
    let (fresh, _) = checker.check("example : (2 : ℚ) + 2 = 4 := by norm_num\n").unwrap();
    assert!(!fresh.has_errors(), "Mathlib types and tactics must exist after recycling: {fresh:?}");
    let second = checker.pid().unwrap();
    assert_ne!(second, first);
    assert!(!crate::test_process_exists(first), "the original resident process was reaped");
    assert_eq!((checker.requests(), checker.restarts(), checker.respawns()), (1, 1, 0));
    let (still_isolated, _) = checker.check("#check nucleus_recycling_probe\n").unwrap();
    assert!(still_isolated.has_errors());
    drop(checker);
    assert!(!crate::test_process_exists(second), "the replacement also has an owned lifetime");
    std::fs::remove_dir_all(scratch).unwrap();
}
