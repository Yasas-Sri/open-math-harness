#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;

fn workspace(root: &Path) -> Workspace {
    Workspace::new(root.to_path_buf())
}

fn fresh(name: &str) -> (PathBuf, Store) {
    let root = scratch(name);
    let store = Store::in_memory().unwrap();
    workspace(&root).init(&store, 1).unwrap();
    (root, store)
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("nucleus_memories_{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn output(id: &str, slug: Option<&str>, summary: &str) -> Stage1Output {
    Stage1Output {
        session_id: id.to_owned(),
        source_updated_at: 1,
        raw_memory: format!("raw for {id}"),
        session_summary: summary.to_owned(),
        session_slug: slug.map(str::to_owned),
        generated_at: 1,
        usage_count: None,
        last_usage: None,
    }
}

#[test]
fn a_fresh_workspace_has_an_empty_baseline_and_an_existing_one_keeps_its_files() {
    let (root, store) = fresh("init");
    assert!(root.join(SUMMARIES).is_dir());
    assert!(!workspace(&root).is_dirty(&store).unwrap(), "a workspace that was just made has no changes");
    assert_eq!(store.published().unwrap(), (String::new(), 0));
    // Idempotent: a second start does not move the baseline.
    std::fs::write(root.join(INDEX), "not yet consolidated").unwrap();
    workspace(&root).init(&store, 2).unwrap();
    assert!(workspace(&root).is_dirty(&store).unwrap());

    // A store that has never seen this directory adopts what is on disk.
    let adopted = Store::in_memory().unwrap();
    workspace(&root).init(&adopted, 3).unwrap();
    assert_eq!(adopted.published().unwrap(), ("not yet consolidated".to_owned(), 3));
}

#[test]
fn nothing_changed_means_no_model_is_called() {
    let (root, store) = fresh("clean");
    let selected = [output("s1", Some("a-thing"), "did a thing")];
    sync(&root, &selected).unwrap();
    assert!(workspace(&root).is_dirty(&store).unwrap(), "the first sync wrote something");
    workspace(&root).commit_baseline(&store, 2).unwrap();

    sync(&root, &selected).unwrap();
    assert!(
        !workspace(&root).is_dirty(&store).unwrap(),
        "the same selection, written again, is not a change"
    );
}

#[test]
fn the_diff_shows_additions_and_removals_and_is_not_baselined() {
    let (root, store) = fresh("drop");
    sync(&root, &[output("s1", Some("kept"), "one"), output("s2", Some("dropped"), "two")]).unwrap();
    workspace(&root).commit_baseline(&store, 2).unwrap();

    sync(&root, &[output("s1", Some("kept"), "one"), output("s3", Some("new"), "three")]).unwrap();
    let diff = std::fs::read_to_string(workspace(&root).write_diff(&store).unwrap()).unwrap();
    assert!(diff.contains("deleted file") && diff.contains("-two"), "the removal is in the diff: {diff}");
    assert!(diff.contains("new file") && diff.contains("+three"), "{diff}");
    assert!(!diff.contains("kept-s1.md"), "an unchanged file is not in the diff: {diff}");
    assert!(!root.join(SUMMARIES).join("dropped-s2.md").exists(), "and it is gone from disk");

    workspace(&root).commit_baseline(&store, 3).unwrap();
    assert!(!root.join(DIFF).exists(), "the prompt artifact is removed before the baseline moves");
    assert!(!workspace(&root).is_dirty(&store).unwrap());
}

#[test]
fn the_order_on_disk_does_not_move_when_ranking_does() {
    let root = scratch("order");
    // The same memories, handed over in the order a ranking would produce.
    sync(&root, &[output("s1", Some("a"), "one"), output("s2", Some("b"), "two")]).unwrap();
    let first = std::fs::read_to_string(root.join(RAW)).unwrap();
    sync(&root, &[output("s2", Some("b"), "two"), output("s1", Some("a"), "one")]).unwrap();
    let second = std::fs::read_to_string(root.join(RAW)).unwrap();
    assert_eq!(first, second, "a memory being cited must not rewrite the file of another");
}

#[test]
fn a_session_without_a_usable_slug_still_gets_a_file_of_its_own() {
    let root = scratch("slug");
    sync(&root, &[output("s1", None, "no slug"), output("s2", Some("  ../../etc  "), "hostile slug")])
        .unwrap();
    assert!(root.join(SUMMARIES).join("s1.md").exists());
    let names: Vec<String> = std::fs::read_dir(root.join(SUMMARIES))
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names.len(), 2, "{names:?}");
    assert!(
        names.iter().all(|n| !n.contains("..") && !n.contains('/')),
        "a slug the model chose cannot escape the directory: {names:?}"
    );
}

#[test]
fn an_empty_selection_says_so_rather_than_leaving_the_last_one_in_place() {
    let (root, store) = fresh("empty");
    sync(&root, &[output("s1", Some("a"), "one")]).unwrap();
    workspace(&root).commit_baseline(&store, 2).unwrap();

    sync(&root, &[]).unwrap();
    let raw = std::fs::read_to_string(root.join(RAW)).unwrap();
    assert!(raw.contains("Nothing is currently selected"));
    assert!(
        workspace(&root).is_dirty(&store).unwrap(),
        "emptying the selection is a change the agent must see"
    );
}

/// Half a merge is worse than the previous index, and the previous index is
/// exactly the baseline. The rows are regenerated from the store next time.
#[test]
fn a_failed_pass_leaves_the_baseline_as_it_was() {
    let (root, store) = fresh("reset");
    sync(&root, &[output("s1", Some("a"), "one")]).unwrap();
    std::fs::write(root.join(INDEX), "- the index as it was\n").unwrap();
    workspace(&root).commit_baseline(&store, 2).unwrap();

    sync(&root, &[output("s1", Some("a"), "one"), output("s2", Some("b"), "two")]).unwrap();
    workspace(&root).write_diff(&store).unwrap();
    std::fs::write(root.join(INDEX), "- half a merge\n").unwrap();

    workspace(&root).reset(&store).unwrap();
    assert_eq!(std::fs::read_to_string(root.join(INDEX)).unwrap(), "- the index as it was\n");
    assert!(!workspace(&root).is_dirty(&store).unwrap(), "the half-written record is gone too");
    assert!(!root.join(SUMMARIES).join("b-s2.md").exists());
    assert!(!root.join(DIFF).exists());
}

#[test]
fn candidates_publish_only_after_validation_and_the_baseline_move() {
    let (root, store) = fresh("candidate");
    std::fs::write(root.join(INDEX), "published").unwrap();
    workspace(&root).commit_baseline(&store, 2).unwrap();
    let next = candidate(&root, "attempt-1").unwrap();
    std::fs::write(next.join(INDEX), "candidate").unwrap();
    std::fs::write(next.join(SUMMARY), vec![b'x'; MAX_ARTIFACT_BYTES as usize + 1]).unwrap();
    assert!(accept_candidate(&root, &next).is_err());
    assert_eq!(std::fs::read_to_string(root.join(INDEX)).unwrap(), "published");
    std::fs::remove_file(next.join(SUMMARY)).unwrap();
    std::fs::write(next.join("unexpected"), "untrusted").unwrap();
    accept_candidate(&root, &next).unwrap();
    assert!(!root.join("unexpected").exists(), "only the two declared artifacts are copied");
    assert_eq!(
        store.published().unwrap(),
        ("published".to_owned(), 2),
        "uncommitted files are not publication"
    );
    workspace(&root).commit_baseline(&store, 5).unwrap();
    assert_eq!(store.published().unwrap(), ("candidate".to_owned(), 5));
    workspace(&root).commit_baseline(&store, 9).unwrap();
    assert_eq!(store.published().unwrap().1, 5, "publication time moves only when the index does");
}

#[test]
fn a_symlink_cannot_be_published_as_an_artifact() {
    let (root, store) = fresh("candidate-link");
    let next = candidate(&root, "attempt").unwrap();
    std::os::unix::fs::symlink("/etc/passwd", next.join(INDEX)).unwrap();
    assert!(accept_candidate(&root, &next).is_err());
    assert!(store.published().unwrap().0.is_empty());
}
