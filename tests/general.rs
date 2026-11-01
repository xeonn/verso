//! Tests for Verso's state that does not need a window or Servo.

use versoview::{
    bookmark::{Bookmark, BookmarkManager},
    tab::next_active_index,
};

#[test]
fn closing_the_active_tab_selects_the_tab_that_takes_its_place() {
    // Tabs [0, 1, 2], tab 1 active and closed: tab 2 moves to index 1.
    assert_eq!(next_active_index(1, 1, 2), Some(1));
}

#[test]
fn closing_the_last_active_tab_selects_the_previous_tab() {
    // Tabs [0, 1, 2], tab 2 active and closed.
    assert_eq!(next_active_index(2, 2, 2), Some(1));
}

#[test]
fn closing_a_tab_before_the_active_one_keeps_the_same_tab_active() {
    // Tabs [0, 1, 2], tab 2 active, tab 0 closed: the active tab is now at index 1.
    assert_eq!(next_active_index(0, 2, 2), Some(1));
}

#[test]
fn closing_a_tab_after_the_active_one_keeps_the_same_index() {
    assert_eq!(next_active_index(2, 0, 2), Some(0));
}

#[test]
fn closing_the_only_tab_leaves_no_active_tab() {
    assert_eq!(next_active_index(0, 0, 0), None);
}

#[test]
fn bookmarks_can_be_added_renamed_and_removed() {
    let mut manager = BookmarkManager::new();
    manager.append_bookmark("Example".into(), "https://example.com/".into());
    let id = manager.bookmarks()[0].id.clone();

    manager.rename_bookmark(id.clone(), "Renamed".into()).unwrap();
    assert_eq!(manager.bookmarks()[0].name, "Renamed");

    manager.remove_bookmark(id.clone()).unwrap();
    assert!(manager.bookmarks().is_empty());
    assert!(manager.remove_bookmark(id).is_err());
}

#[test]
fn bookmarks_serialize_as_the_pages_expect() {
    let bookmark = Bookmark::new("Example".into(), "https://example.com/".into());
    let json = serde_json::to_value(vec![bookmark]).unwrap();
    assert_eq!(json[0]["name"], "Example");
    assert_eq!(json[0]["url"], "https://example.com/");
}
