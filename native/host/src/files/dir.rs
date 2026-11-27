//! files.dir: server's DirJob.sh in Rust. One folder of a thread's project: folders then files,
//! "d|f <tab> size <tab> name" lines, hidden names (glob `*`) left out, symlinked folders left out,
//! files by what they point at, 2000 of each, byte order (the shell's glob in the C locale).
//!
//! A listing costs a stat per entry, and a folder with three thousand files in it costs 60 ms to list: the Files page is a picture, made on every
//! state change of the app, and its folder watch lists the same folder every 400 ms (`files::watch_dir`). So the pictures share one walk (`list`, for
//! a moment: `LISTED`) and the watch reads the folder itself (`list_now`), which is what notices a file written in place and growing.
use super::fm;
use std::path::{Path, PathBuf};

/// DirJob.sent on refusal (since PR #960): one message for a folder that is missing, a link, or outside the project.
pub const UNAVAILABLE: &str = "that folder is unavailable or outside this project";

pub const MAX: usize = 2000;

/// How long a folder's listing is kept for the pictures and the watch that ask again (`list`). Short enough that a file written in place (a log
/// growing, a build's output) shows its new size in the next picture after it, and far under the folder watch's own 400 ms poll, which is what
/// notices the change and makes the app draw again.
const LISTED_MS: u64 = 250;
/// The most folders kept at once (the Files page shows one; a split view or a phone may show a few more).
const LISTED_KEEP: usize = 16;

/// The listing text, or the shell's one-line error ("no such folder").
pub fn list(root: &Path, rel: &str) -> Result<String, &'static str> {
    let dir = fm::contained(root, rel).ok_or(UNAVAILABLE)?;
    // a folder whose own time moved had a file added, removed or renamed: listed again at once, whatever was kept
    let stamp = std::fs::metadata(&dir).and_then(|m| m.modified()).ok();
    if let Some(t) = stamp.as_ref().and_then(|s| kept(&dir, *s)) { return Ok(t) }
    let text = walk(&dir)?;
    if let Some(s) = stamp { keep(&dir, s, &text) }
    Ok(text)
}

/// The listing as the folder is now, for the folder watch: its poll is what notices a file written in place (a log growing, a build's output), whose
/// size the folder's own time does not report, so it reads the folder itself rather than what `list` kept for the pictures.
pub fn list_now(root: &Path, rel: &str) -> Result<String, &'static str> {
    let dir = fm::contained(root, rel).ok_or(UNAVAILABLE)?;
    walk(&dir)
}

/// One folder walked: its text, exactly as the client draws it (and as `watch_dir` hashes it).
fn walk(dir: &Path) -> Result<String, &'static str> {
    let rd = std::fs::read_dir(dir).map_err(|_| UNAVAILABLE)?;
    // a name that is not UTF-8 is never listed, and sorting 2000 names with a lossy conversion apiece costs more than the walk does: the names are
    // read as strings once, then sorted in JS order (UTF-16 code units) without converting anything again
    let mut names: Vec<String> = rd.filter_map(|e| e.ok()).filter_map(|e| e.file_name().into_string().ok()).collect();
    names.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
    let (mut dirs, mut files) = (String::new(), String::new());
    let (mut nd, mut nf) = (0, 0);
    for name in &names {
        // `*` skips dotfiles; KiCad writes `~<name>.lck` temporary lock files beside open documents;
        // a name holding a newline, tab, CR or backslash would break the listing format: left out
        if name.starts_with('.') || (name.starts_with('~') && name.ends_with(".lck")) || name.contains(['\n', '\t', '\r', '\\']) { continue }
        let p = dir.join(name);
        // symlinks and non-regular entries are never exposed (a link to a file is not listed either)
        let Ok(lm) = std::fs::symlink_metadata(&p) else { continue };
        if lm.is_dir() { if nd < MAX { dirs.push_str(&format!("d\t0\t{name}\n")); nd += 1 } }
        else if lm.is_file() && nf < MAX { files.push_str(&format!("f\t{}\t{name}\n", lm.len())); nf += 1 }
        // both kinds full: nothing any later name could add
        if nd >= MAX && nf >= MAX { break }
    }
    Ok(dirs + &files)
}

/// The listings kept for a moment: the folder, the time it had when it was walked, when that was, and what it read.
static LISTED: std::sync::LazyLock<std::sync::Mutex<Vec<(PathBuf, std::time::SystemTime, std::time::Instant, String)>>> =
    std::sync::LazyLock::new(Default::default);
/// Test aid: how many times a folder was walked on this thread (`list` answering from what it kept does not count).
#[cfg(test)] thread_local! { static WALKS: std::cell::Cell<usize> = std::cell::Cell::new(0); }
#[cfg(test)] pub fn walks() -> usize { WALKS.with(|w| w.get()) }

fn kept(dir: &Path, stamp: std::time::SystemTime) -> Option<String> {
    let mut l = LISTED.lock().unwrap();
    let i = l.iter().position(|e| e.0 == dir && e.1 == stamp)?;
    if l[i].2.elapsed().as_millis() as u64 >= LISTED_MS { l.remove(i); return None }
    Some(l[i].3.clone())
}

fn keep(dir: &Path, stamp: std::time::SystemTime, text: &str) {
    let mut l = LISTED.lock().unwrap();
    #[cfg(test)] WALKS.with(|w| w.set(w.get() + 1));
    l.retain(|e| !(e.0 == dir && e.1 == stamp));
    if l.len() >= LISTED_KEEP { if let Some(old) = l.iter().enumerate().min_by_key(|(_, e)| e.2).map(|(i, _)| i) { l.remove(old); } }
    l.push((dir.to_path_buf(), stamp, std::time::Instant::now(), text.to_string()));
}

#[cfg(test)]
mod tests {
    use super::*; use crate::plat::fs::symlink;
    fn tmp(n: &str) -> crate::plat::tmp::TempDir { crate::plat::tmp::dir(&format!("dir-{n}")) }
    #[test] fn lists_folders_then_files_hides_dotfiles_and_symlinked_folders() {
        let b = tmp("l"); let r = b.join("r"); std::fs::create_dir_all(r.join("zdir")).unwrap(); std::fs::create_dir_all(r.join("adir")).unwrap(); std::fs::create_dir_all(r.join(".git")).unwrap();
        std::fs::write(r.join("b.txt"), "12345").unwrap(); std::fs::write(r.join("a.kicad_pcb"), "").unwrap(); std::fs::write(r.join(".env"), "k").unwrap(); std::fs::write(r.join("bad\tname"), "x").unwrap();
        std::fs::create_dir_all(b.join("out")).unwrap(); symlink(b.join("out"), r.join("linkdir")).unwrap(); symlink(r.join("b.txt"), r.join("linkfile")).unwrap(); symlink("/nonexistent", r.join("dangling")).unwrap();
        assert_eq!(list(&r, "").unwrap(), "d\t0\tadir\nd\t0\tzdir\nf\t0\ta.kicad_pcb\nf\t5\tb.txt\n");
        assert_eq!(list(&r, "adir").unwrap(), ""); let _ = std::fs::remove_dir_all(&b);
    }
    #[test] fn escapes_are_no_such_folder() {
        let b = tmp("e"); let r = b.join("r"); std::fs::create_dir_all(&r).unwrap(); symlink("/etc", r.join("etc")).unwrap();
        for p in ["..", "etc", "/etc", "x/../..", "missing", "a\0b"] { assert_eq!(list(&r, p), Err(UNAVAILABLE), "{p}") }
        assert_eq!(list(&b.join("none"), ""), Err(UNAVAILABLE)); let _ = std::fs::remove_dir_all(&b);
    }
    #[test] fn bounded_at_2000_each() {
        let b = tmp("m"); for i in 0..2105 { std::fs::write(b.join(format!("f{i:05}")), "").unwrap() } for i in 0..2005 { std::fs::create_dir(b.join(format!("d{i:05}"))).unwrap() }
        let t = list(&b, "").unwrap(); assert_eq!(t.lines().filter(|l| l.starts_with("d\t")).count(), 2000); assert_eq!(t.lines().filter(|l| l.starts_with("f\t")).count(), 2000); let _ = std::fs::remove_dir_all(&b);
    }
    #[test] fn hides_kicad_lock_files() {
        let b = tmp("lck"); std::fs::write(b.join("a.kicad_pcb"), "").unwrap(); std::fs::write(b.join("~a.kicad_pcb.lck"), "{}").unwrap();
        std::fs::write(b.join("~a.kicad_sch.lck"), "{}").unwrap(); std::fs::write(b.join("notes.lck"), "ok").unwrap();
        assert_eq!(list(&b, "").unwrap(), "f\t0\ta.kicad_pcb\nf\t2\tnotes.lck\n");
        let _ = std::fs::remove_dir_all(&b);
    }
    /// A burst of state changes (an agent's turn) over a folder big enough that its walk is visible: what the pictures cost together. Printed.
    #[test] fn a_burst_of_pictures_walks_a_big_folder_once() {
        let b = tmp("burst"); for i in 0..3000 { std::fs::write(b.join(format!("file{i}.txt")), "x").unwrap() }
        let n = 20; let before = walks();
        let t = std::time::Instant::now();
        for _ in 0..n { assert!(list(&b, "").unwrap().lines().count() >= 2000) }
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        let walked = walks() - before;
        eprintln!("20 pictures over a 3000-file folder: {ms:.0} ms, {walked} walks (one walk is ~60 ms here)");
        assert_eq!(walked, 1, "the burst shares one walk of the folder");
        let _ = std::fs::remove_dir_all(&b);
    }
    /// A picture is made on every state change and the folder watch lists the same folder every 400 ms: one walk of a folder serves them both, until the
    /// folder changes (a file added, removed or renamed moves its own time) or the moment kept for a file written in place is over.
    #[test] fn one_walk_serves_the_pictures_and_the_watch_until_the_folder_moves() {
        let b = tmp("share"); std::fs::write(b.join("a.txt"), "one").unwrap();
        let before = walks();
        let first = list(&b, "").unwrap();
        assert_eq!(walks(), before + 1, "the first ask walks the folder");
        assert_eq!(list(&b, "").unwrap(), first);
        assert_eq!(walks(), before + 1, "the pictures that follow share that walk");
        // a file added: the folder's own time moves, so the next ask walks it and sees the file
        std::fs::write(b.join("b.txt"), "two").unwrap();
        let after_add = list(&b, "").unwrap();
        assert!(after_add.contains("b.txt"), "{after_add}");
        assert_eq!(walks(), before + 2);
        // a file written in place (a log growing): the folder's time does not move, so it shows its old size until the moment kept is over
        std::fs::write(b.join("a.txt"), "longer").unwrap();
        let same = list(&b, "").unwrap();
        assert_eq!(same, after_add, "an in-place write is not a listing change");
        std::thread::sleep(std::time::Duration::from_millis(LISTED_MS + 50));
        let later = list(&b, "").unwrap();
        assert!(later.contains("f\t6\ta.txt"), "past the moment kept, the size is read again: {later}");
        assert_eq!(walks(), before + 3);
        let _ = std::fs::remove_dir_all(&b);
    }
}
