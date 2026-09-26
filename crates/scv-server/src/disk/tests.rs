//! Unit tests for `src/disk.rs`.

use super::*;

#[test]
fn free_space_is_read_for_missing_paths_through_their_parent() {
    let home = tempfile::tempdir().unwrap();
    let free = free_percent(home.path()).unwrap();
    assert!(free <= 100);
    assert_eq!(free_percent(&home.path().join("not/yet/made")), Some(free));
    let paths = vec![home.path().to_path_buf(), home.path().join("x")];
    assert_eq!(lowest(&paths).unwrap().0, free);
    assert_eq!(lowest(&[]), None);
}

#[test]
fn the_owner_hears_once_when_space_runs_low_and_once_when_it_is_back() {
    let disk = |free| Some((free, PathBuf::from("/home/u/.scv/history")));
    let (low, text) = step(false, 20, disk(12).as_ref()).unwrap();
    assert!(low);
    assert!(
        text.contains("/home/u/.scv/history has only 12% free, below the 20%"),
        "{text}"
    );
    // Still low: nothing more to say.
    assert_eq!(step(true, 20, disk(5).as_ref()), None);
    // Back at the floor is not enough: saving resumes two points above it.
    assert_eq!(step(true, 20, disk(20).as_ref()), None);
    assert_eq!(step(true, 20, disk(21).as_ref()), None);
    let (low, text) = step(true, 20, disk(22).as_ref()).unwrap();
    assert!(!low);
    assert!(text.contains("again"));
    assert_eq!(step(false, 20, disk(50).as_ref()), None);
    // A floor of 0 turns the check off, and a disk that cannot be read
    // counts as fine.
    assert_eq!(step(false, 0, disk(0).as_ref()), None);
    assert!(!step(true, 0, disk(0).as_ref()).unwrap().0);
    assert_eq!(step(false, 20, None), None);
}
