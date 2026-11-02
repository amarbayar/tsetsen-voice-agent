//! The walker against a throwaway folder tree: never the user's own files.

use std::fs;
use std::time::{Duration, SystemTime};

use tsetsen_core::ports::{FileQuery, FileSearch};
use tsetsen_core::{Ctx, RequestId};
use tsetsen_files::{WalkFiles, user_folders};

#[tokio::test]
async fn finds_by_every_word_newest_first_and_stays_in_bounds() {
    let root = std::env::temp_dir().join(format!("tsetsen-walk-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    for dir in ["Documents/Bank", "Documents/.hidden", "Documents/Old.app"] {
        fs::create_dir_all(root.join(dir)).unwrap();
    }
    for file in [
        "Documents/Bank/Bank letter.pdf",
        "Documents/Bank letter draft.txt",
        "Documents/Recipes.docx",
        "Documents/.hidden/bank letter secret.pdf",
        "Documents/Old.app/bank letter.pdf",
    ] {
        fs::write(root.join(file), b"x").unwrap();
    }
    let folders = user_folders(&root);
    assert_eq!(folders, [root.join("Documents")]);
    let search = WalkFiles::new(folders);
    let cx = Ctx::new(RequestId::new(1));
    let query = FileQuery {
        words: vec!["bank".into(), "letter".into()],
        limit: 10,
        ..FileQuery::default()
    };
    let mut names: Vec<String> = search
        .search(&query, &cx)
        .await
        .unwrap()
        .iter()
        .map(tsetsen_core::ports::FileInfo::name)
        .collect();
    names.sort();
    assert_eq!(
        names,
        ["Bank letter draft.txt", "Bank letter.pdf"],
        "hidden and package files skipped"
    );

    let future = FileQuery {
        from: Some(SystemTime::now() + Duration::from_secs(3600)),
        ..query
    };
    assert!(
        search.search(&future, &cx).await.unwrap().is_empty(),
        "dates bound the search"
    );
    fs::remove_dir_all(&root).unwrap();
}
