use crate::error::{BackendError, Result};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

#[derive(Serialize, Deserialize)]
struct Journal {
    committed: bool,
    entries: Vec<Entry>,
}
#[derive(Serialize, Deserialize)]
struct Entry {
    target: PathBuf,
    old: Option<Vec<u8>>,
    new: Vec<u8>,
}

fn journal_path(base: &Path) -> PathBuf {
    base.join(".voicemixer-save-journal.json")
}

fn allowed(base: &Path, target: &Path) -> Result<()> {
    if target
        .components()
        .any(|part| matches!(part, std::path::Component::ParentDir))
    {
        return Err(BackendError::InvalidOperation(
            "保存対象に親ディレクトリ参照は使用できません".into(),
        ));
    }
    let base = base.canonicalize()?;
    let parent = target
        .parent()
        .ok_or_else(|| BackendError::InvalidOperation("保存先が不正です".into()))?;
    let resolved = if target.exists() {
        target.canonicalize()?
    } else {
        parent.canonicalize()?.join(target.file_name().unwrap())
    };
    if !resolved.starts_with(&base) || resolved == base {
        return Err(BackendError::InvalidOperation(
            "保存対象が設定ディレクトリ外です".into(),
        ));
    }
    Ok(())
}

pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!("{}.txn", uuid::Uuid::new_v4()));
    let mut file = fs::File::create(&tmp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    if let Err(source) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(BackendError::ConfigIo {
            path: path.to_path_buf(),
            source,
        });
    }
    Ok(())
}

fn publish_journal(path: &Path, journal: &Journal) -> Result<()> {
    atomic_write(path, &serde_json::to_vec(journal)?)
}

/// Two-file write-ahead journal. Before the commit marker recovery restores
/// both old bytes; after it, recovery rolls both targets forward to new bytes.
pub fn recover(base: &Path) -> Result<()> {
    let _guard = STORAGE_LOCK.lock();
    let path = journal_path(base);
    if !path.exists() {
        return Ok(());
    }
    let journal: Journal =
        serde_json::from_slice(&fs::read(&path).map_err(|source| BackendError::ConfigIo {
            path: path.clone(),
            source,
        })?)?;
    for entry in &journal.entries {
        allowed(base, &entry.target)?;
    }
    for entry in &journal.entries {
        if let Some(parent) = entry.target.parent() {
            fs::create_dir_all(parent)?;
        }
        if journal.committed {
            atomic_write(&entry.target, &entry.new)?;
        } else if let Some(old) = &entry.old {
            atomic_write(&entry.target, old)?;
        } else if entry.target.exists() {
            fs::remove_file(&entry.target)?;
        }
    }
    fs::remove_file(path)?;
    Ok(())
}

pub fn commit_pair(base: &Path, first: (&Path, Vec<u8>), second: (&Path, Vec<u8>)) -> Result<()> {
    commit_entries(base, vec![first, second])
}

static STORAGE_LOCK: parking_lot::ReentrantMutex<()> = parking_lot::ReentrantMutex::new(());

/// コミット済みjournalが正本。反映が失敗しても「未コミット」として巻き戻さず、次回読込で再実行する。
pub fn commit_single(base: &Path, target: &Path, bytes: Vec<u8>) -> Result<()> {
    commit_entries(base, vec![(target, bytes)])
}

fn commit_entries(base: &Path, entries: Vec<(&Path, Vec<u8>)>) -> Result<()> {
    let _guard = STORAGE_LOCK.lock();
    fs::create_dir_all(base)?;
    recover(base)?;
    let journal_path = journal_path(base);
    for (target, _) in &entries {
        allowed(base, target)?;
    }
    let old = |path: &Path| match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(BackendError::ConfigIo {
            path: path.to_path_buf(),
            source,
        }),
    };
    let mut journal = Journal {
        committed: false,
        entries: entries
            .into_iter()
            .map(|(target, new)| {
                Ok(Entry {
                    target: target.to_path_buf(),
                    old: old(target)?,
                    new,
                })
            })
            .collect::<Result<Vec<_>>>()?,
    };
    failpoint("before_prepare")?;
    publish_journal(&journal_path, &journal)?;
    failpoint("after_prepare")?;
    // Marking committed is the durable linearization point. A restart before
    // this marker restores old bytes; after it recovery completes new bytes.
    journal.committed = true;
    publish_journal(&journal_path, &journal)?;
    // この点以降は永続commit済み。反映失敗を呼出元のrollbackへ渡してはいけない。
    let apply = (|| -> Result<()> {
        failpoint("after_commit")?;
        for (index, entry) in journal.entries.iter().enumerate() {
            atomic_write(&entry.target, &entry.new)?;
            if index == 0 {
                failpoint("after_first_target")?;
            }
        }
        failpoint("before_cleanup")?;
        fs::remove_file(&journal_path)?;
        Ok(())
    })();
    if let Err(error) = apply {
        eprintln!("保存はcommit済みです。次回読込でjournalを再反映します: {error}");
    }
    Ok(())
}

#[cfg(test)]
thread_local! { static FAILURE: std::cell::Cell<Option<&'static str>> = const { std::cell::Cell::new(None) }; }
fn failpoint(point: &str) -> Result<()> {
    #[cfg(test)]
    if FAILURE.with(|failure| failure.get() == Some(point)) {
        return Err(BackendError::InvalidOperation(format!("注入失敗: {point}")));
    }
    let _ = point;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_commit_boundary_recovers_an_entire_generation() {
        for point in [
            "before_prepare",
            "after_prepare",
            "after_commit",
            "after_first_target",
            "before_cleanup",
        ] {
            let temp = tempfile::tempdir().unwrap();
            let first = temp.path().join("config.json");
            let second = temp.path().join("profile.json");
            fs::write(&first, b"old-a").unwrap();
            fs::write(&second, b"old-b").unwrap();
            FAILURE.with(|failure| failure.set(Some(point)));
            let result = commit_pair(
                temp.path(),
                (&first, b"new-a".to_vec()),
                (&second, b"new-b".to_vec()),
            );
            FAILURE.with(|failure| failure.set(None));
            let committed = !matches!(point, "before_prepare" | "after_prepare");
            assert_eq!(result.is_ok(), committed, "{point}");
            recover(temp.path()).unwrap();
            assert_eq!(
                fs::read(&first).unwrap(),
                if committed { b"new-a" } else { b"old-a" }
            );
            assert_eq!(
                fs::read(&second).unwrap(),
                if committed { b"new-b" } else { b"old-b" }
            );
            assert!(!journal_path(temp.path()).exists());
        }
    }

    #[test]
    fn parent_traversal_is_rejected_before_any_file_change() {
        let temp = tempfile::tempdir().unwrap();
        let unsafe_path = temp.path().join("../outside.json");
        assert!(commit_single(temp.path(), &unsafe_path, b"bad".to_vec()).is_err());
        assert!(!journal_path(temp.path()).exists());
    }

    fn write_journal(base: &Path, committed: bool, first: &Path, second: &Path) {
        let journal = Journal {
            committed,
            entries: vec![
                Entry {
                    target: first.to_path_buf(),
                    old: Some(b"old-first".to_vec()),
                    new: b"new-first".to_vec(),
                },
                Entry {
                    target: second.to_path_buf(),
                    old: Some(b"old-second".to_vec()),
                    new: b"new-second".to_vec(),
                },
            ],
        };
        fs::write(journal_path(base), serde_json::to_vec(&journal).unwrap()).unwrap();
    }

    #[test]
    fn prepared_recovery_restores_both_old_payloads() {
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("first.json");
        let second = temp.path().join("second.json");
        fs::write(&first, b"torn").unwrap();
        fs::write(&second, b"torn").unwrap();
        write_journal(temp.path(), false, &first, &second);
        recover(temp.path()).unwrap();
        assert_eq!(fs::read(&first).unwrap(), b"old-first");
        assert_eq!(fs::read(&second).unwrap(), b"old-second");
        assert!(!journal_path(temp.path()).exists());
    }

    #[test]
    fn committed_recovery_rolls_both_payloads_forward() {
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("first.json");
        let second = temp.path().join("second.json");
        write_journal(temp.path(), true, &first, &second);
        recover(temp.path()).unwrap();
        assert_eq!(fs::read(&first).unwrap(), b"new-first");
        assert_eq!(fs::read(&second).unwrap(), b"new-second");
    }

    #[test]
    fn commit_pair_writes_both_payloads_and_cleans_journal() {
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("one.json");
        let second = temp.path().join("two.json");
        fs::write(&first, b"before-one").unwrap();
        fs::write(&second, b"before-two").unwrap();
        commit_pair(
            temp.path(),
            (&first, b"after-one".to_vec()),
            (&second, b"after-two".to_vec()),
        )
        .unwrap();
        assert_eq!(fs::read(&first).unwrap(), b"after-one");
        assert_eq!(fs::read(&second).unwrap(), b"after-two");
        assert!(!journal_path(temp.path()).exists());
    }
}
