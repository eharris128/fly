//! Phone screenshot drop — storage, sniffing, and prompt composition
//! (phone-screenshot-drop U1–U3).
//!
//! This module owns everything between "bytes arrive on the wire" and "text is
//! ready for the pane": where the image lands (this file's path helpers),
//! what format it actually is (`sniff_image`, U2), how it is written durably
//! (`DropStore`, U2), and the exact text the agent sees (`compose_drop_prompt`,
//! U3). The route that drives it lives in `server.rs` (U6).
//!
//! Everything here is pure or filesystem-only — no Tauri, no PTY, no HTTP — so
//! it is unit-testable without a running app.

use std::fmt;
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use chrono::{DateTime, Utc};

use crate::automations::store::{create_private_dir, sync_parent_dir};

/// Bytes of the body buffered before any file is created — enough for every
/// pattern in [`sniff_image`] (the longest, an ISO-BMFF `ftyp` major brand,
/// ends at byte 12).
const SNIFF_BYTES: usize = 16;

/// Copy buffer for the streaming body write. Fixed size is the point: fly's own
/// memory stays O(1) regardless of image size (KTD1).
const COPY_CHUNK: usize = 64 * 1024;

/// Prefix marking a temp file as fly's, so the startup sweep can distinguish
/// crash residue from a user's own files — the drop directory is somewhere the
/// user browses (KTD4), so the sweep must never touch anything else.
const TEMP_PREFIX: &str = ".fly-drop-tmp-";

/// Age past which a prefixed temp file is presumed crash residue and swept
/// (KTD7). Comfortably longer than any real upload.
const TEMP_STALE_AFTER: Duration = Duration::from_secs(5 * 60);

/// Why a configured `feed.dropDir` could not be turned into a usable absolute
/// path. Every variant is a *refusal*, never a silent fallback: resolving a bad
/// shape against the process cwd would put screenshots in `/` for a GUI launched
/// from a desktop file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DropDirError {
    /// The configured value was empty or entirely whitespace.
    Empty,
    /// Neither absolute nor tilde-prefixed — e.g. `inbox` or `./shots`.
    Relative,
    /// `~user/...`. Expanding this needs a passwd lookup fly does not do.
    ForeignHome,
    /// Tilde-prefixed but `$HOME` could not be determined.
    NoHome,
}

impl fmt::Display for DropDirError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let msg = match self {
            Self::Empty => "feed.dropDir is empty",
            Self::Relative => "feed.dropDir must be absolute or start with `~/`",
            Self::ForeignHome => "feed.dropDir uses `~user/`, which fly does not expand",
            Self::NoHome => "feed.dropDir starts with `~` but $HOME is unset",
        };
        f.write_str(msg)
    }
}

/// Expand a leading `~` against `home` and require the result be absolute.
///
/// Pure — `home` is injected rather than read from the environment, so the
/// tests below pin the expansion without touching `$HOME`.
///
/// This is deliberately **not** applied at deserialization. `set_config`
/// round-trips the whole `Config` back to disk, so an expansion baked into the
/// in-memory struct would be persisted the first time the settings menu saves
/// anything, silently rewriting the user's `~/projects/inbox` and freezing
/// `$HOME` into their config file. See [`crate::config::FeedConfig::drop_dir`].
///
/// Absoluteness is enforced here rather than in a second validator so there is
/// exactly one place that decides what a legal `dropDir` looks like.
pub fn expand_tilde(raw: &str, home: Option<&Path>) -> Result<PathBuf, DropDirError> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(DropDirError::Empty);
    }
    let Some(rest) = raw.strip_prefix('~') else {
        // No tilde: the only other legal shape is an absolute path.
        return if Path::new(raw).is_absolute() {
            Ok(PathBuf::from(raw))
        } else {
            Err(DropDirError::Relative)
        };
    };
    // `~`, `~/…`, or `~user/…` — only the first two are ours to expand.
    let tail = match rest {
        "" => None,
        r if r.starts_with('/') => Some(r.trim_start_matches('/')),
        _ => return Err(DropDirError::ForeignHome),
    };
    let home = home.ok_or(DropDirError::NoHome)?;
    Ok(match tail {
        // `~/` with nothing after it is still just `$HOME`.
        Some(t) if !t.is_empty() => home.join(t),
        _ => home.to_path_buf(),
    })
}

/// Resolve the effective drop directory: the configured value if set, else
/// `<data root>/inbox` (KTD4).
///
/// The unconfigured default goes under the data root — and so *does* stay
/// per-flavor isolated — while an explicitly configured directory is taken at
/// face value. That asymmetry is intended: the default has to put files
/// somewhere defensible, but a user who names a directory means that directory.
pub fn resolve_drop_dir(
    configured: Option<&str>,
    home: Option<&Path>,
    data_root: &Path,
) -> Result<PathBuf, DropDirError> {
    match configured {
        Some(raw) => expand_tilde(raw, home),
        None => Ok(data_root.join("inbox")),
    }
}

/// An image format fly is willing to store, as determined by the *bytes* —
/// never by the client-declared content type, which is attacker-controlled and
/// on iOS sometimes simply wrong (R16, KTD8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageKind {
    Png,
    Jpeg,
    Gif,
    Webp,
    /// HEIC/HEIF. Stored, not rejected: iOS *screenshots* are PNG so the common
    /// path is unaffected, but a camera photo may arrive as HEIC. Refusing a
    /// file fly can perfectly well store, on a guess about what the agent's
    /// image reader supports, is the wrong call — the page warns instead.
    Heic,
    Avif,
}

impl ImageKind {
    /// The stored extension. An exhaustive match over a fixed set of literals,
    /// so no client-supplied string can ever reach the path (R16).
    pub fn extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpg",
            Self::Gif => "gif",
            Self::Webp => "webp",
            Self::Heic => "heic",
            Self::Avif => "avif",
        }
    }
}

/// ISO-BMFF `ftyp` major brands that mean HEIC/HEIF.
const HEIC_BRANDS: [&[u8; 4]; 10] = [
    b"heic", b"heix", b"hevc", b"hevx", b"heim", b"heis", b"hevm", b"hevs", b"mif1", b"msf1",
];

/// ISO-BMFF `ftyp` major brands that mean AVIF.
const AVIF_BRANDS: [&[u8; 4]; 2] = [b"avif", b"avis"];

/// Identify an image from its leading bytes, or abstain.
///
/// Total and abstain-on-surprise: an unrecognized prefix returns `None` rather
/// than guessing, matching the posture of every other detector in this
/// codebase. PNG/JPEG/GIF/WebP follow the WHATWG MIME-sniffing image patterns;
/// HEIC and AVIF are not in that spec and are matched by their ISO-BMFF `ftyp`
/// major brand, which is where those formats actually declare themselves.
pub fn sniff_image(bytes: &[u8]) -> Option<ImageKind> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some(ImageKind::Png);
    }
    if bytes.starts_with(b"\xff\xd8\xff") {
        return Some(ImageKind::Jpeg);
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some(ImageKind::Gif);
    }
    // RIFF is a container family — the WebP marker is at bytes 8..12, so a WAV
    // or AVI must not match on the `RIFF` prefix alone.
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        return Some(ImageKind::Webp);
    }
    // ISO-BMFF: a leading box whose type is `ftyp`, with the major brand next.
    if bytes.len() >= 12 && &bytes[4..8] == b"ftyp" {
        let brand: &[u8] = &bytes[8..12];
        if HEIC_BRANDS.iter().any(|b| b.as_slice() == brand) {
            return Some(ImageKind::Heic);
        }
        if AVIF_BRANDS.iter().any(|b| b.as_slice() == brand) {
            return Some(ImageKind::Avif);
        }
    }
    None
}

/// Mint the stored filename: `<UTC timestamp>-<16 hex chars>.<ext>`.
///
/// Both inputs are injected so the function is deterministic under test. **No
/// client-supplied component participates** (R16) — not the declared filename,
/// not the content type, not the caption. The extension comes from the sniffed
/// [`ImageKind`], so the whole name is drawn from a fixed alphabet with no path
/// separator and no `..` possible.
///
/// The random half also makes names globally unique across the stable and dev
/// flavors, which is what lets them share one directory (KTD4).
pub fn mint_filename(now: DateTime<Utc>, rand: [u8; 8], kind: ImageKind) -> String {
    let mut hex = String::with_capacity(16);
    for b in rand {
        use std::fmt::Write;
        let _ = write!(hex, "{b:02x}");
    }
    format!(
        "{}-{}.{}",
        now.format("%Y%m%dT%H%M%SZ"),
        hex,
        kind.extension()
    )
}

/// Why a store attempt failed. The route maps each to a distinct wire code, so
/// the phone can tell "too big" from "not an image" from "the disk is broken"
/// (R5, R12, R16, R17).
#[derive(Debug)]
pub enum DropError {
    /// The body exceeded the cap. Nothing is retained.
    Oversize,
    /// The leading bytes matched no known image format.
    BadFormat,
    /// The directory is unwritable, the disk is full, or a rename failed.
    Storage(io::Error),
}

impl fmt::Display for DropError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Oversize => f.write_str("image exceeds the configured size cap"),
            Self::BadFormat => f.write_str("body is not a recognized image format"),
            Self::Storage(e) => write!(f, "drop storage failed: {e}"),
        }
    }
}

impl From<io::Error> for DropError {
    fn from(e: io::Error) -> Self {
        Self::Storage(e)
    }
}

/// The destination directory for phone drops, with the durable-write discipline
/// the rest of fly's stores use: directory `0700`, file `0600`, temp-in-the-same
/// directory then rename, `sync_all` before the rename.
///
/// **Note what is deliberately *not* reused.**
/// [`crate::automations::store::write_atomic_owner_only`] takes a fully
/// materialized `&[u8]`; calling it with an image body would reintroduce the
/// 25 MiB buffer KTD1 exists to avoid. This type writes its own streaming temp
/// file while reusing that module's `create_private_dir` and `sync_parent_dir`
/// directly. The atomic writer is the *discipline* precedent here, not the
/// primitive.
pub struct DropStore {
    dir: PathBuf,
}

impl DropStore {
    /// Create the directory if absent, canonicalize it once, and sweep stale
    /// crash residue.
    ///
    /// A failure here is **retained and reported per request**, never a reason
    /// to refuse to start the feed listener — AE8 depends on an unwritable drop
    /// directory surfacing as a `storageFailed` response rather than taking the
    /// whole feed (and the dashboard that reads it) down with it. See the
    /// construction site in `lib.rs`.
    pub fn new(dir: &Path) -> io::Result<Self> {
        create_private_dir(dir)?;
        // Canonicalize once, so every later `starts_with` scope check compares
        // resolved paths (the `read_bundle_scoped` precedent).
        let dir = fs::canonicalize(dir)?;
        sweep_stale_temps(&dir);
        Ok(Self { dir })
    }

    /// The canonicalized destination directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Stream `reader` into a temp file in this directory, refusing anything
    /// over `cap` bytes or whose leading bytes are not an image.
    ///
    /// Ordering is load-bearing (KTD7): the first [`SNIFF_BYTES`] are buffered
    /// and matched *before* any file is created, so a non-image body never
    /// touches the filesystem at all. Past that point the returned
    /// [`StoredImage`] owns the temp file and unlinks it on drop, so an early
    /// return anywhere in the caller cannot orphan one.
    pub fn store(&self, reader: &mut impl Read, cap: u64) -> Result<StoredImage, DropError> {
        let mut head = [0u8; SNIFF_BYTES];
        let head_len = read_up_to(reader, &mut head)?;
        let head = &head[..head_len];

        // Refuse before creating anything. A body shorter than the shortest
        // pattern simply fails to match, which is the same abstention.
        let kind = sniff_image(head).ok_or(DropError::BadFormat)?;
        if head_len as u64 > cap {
            return Err(DropError::Oversize);
        }

        let (temp_path, mut file) = self.create_temp()?;
        // From here on `guard` owns the unlink, so every `?` below cleans up.
        let guard = StoredImage {
            temp: Some(temp_path),
            dest: self.dir.join(mint_filename(Utc::now(), random_bytes(), kind)),
            kind,
        };

        file.write_all(head)?;
        let mut written = head_len as u64;

        // `take(remaining + 1)` bounds the read at one byte past the cap: if
        // that extra byte materializes the body is oversize, and we know it
        // without having read (or written) anything more.
        let remaining = cap - written;
        let mut bounded = reader.take(remaining.saturating_add(1));
        let mut buf = vec![0u8; COPY_CHUNK];
        loop {
            let n = bounded.read(&mut buf)?;
            if n == 0 {
                break;
            }
            written = written.saturating_add(n as u64);
            if written > cap {
                // `guard` unlinks the partial temp file as it drops.
                return Err(DropError::Oversize);
            }
            file.write_all(&buf[..n])?;
        }

        // Durability before delivery is not optional here: the agent is about to
        // be asked to read this path, immediately.
        file.sync_all()?;
        Ok(guard)
    }

    /// Create a uniquely-named `0600` temp file in the destination directory
    /// (same filesystem, so the later rename is atomic).
    fn create_temp(&self) -> io::Result<(PathBuf, fs::File)> {
        let mut hex = String::with_capacity(16);
        for b in random_bytes() {
            use std::fmt::Write;
            let _ = write!(hex, "{b:02x}");
        }
        let path = self.dir.join(format!("{TEMP_PREFIX}{hex}"));
        let file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        Ok((path, file))
    }
}

/// A stored-but-not-yet-published image. Holds the temp file until
/// [`commit`](StoredImage::commit) renames it into place; dropping it
/// un-committed unlinks, which is what makes every refusal path in the route
/// leak-free without each one remembering to clean up (KTD7).
#[derive(Debug)]
pub struct StoredImage {
    temp: Option<PathBuf>,
    dest: PathBuf,
    kind: ImageKind,
}

impl StoredImage {
    /// The path this image *will* occupy once committed. Available before the
    /// commit so the prompt can be composed while the refusal checks run.
    pub fn dest(&self) -> &Path {
        &self.dest
    }

    /// The sniffed format.
    pub fn kind(&self) -> ImageKind {
        self.kind
    }

    /// Atomically publish the image, returning its final path.
    pub fn commit(mut self) -> Result<PathBuf, DropError> {
        let Some(temp) = self.temp.take() else {
            return Err(DropError::Storage(io::Error::new(
                io::ErrorKind::Other,
                "image already committed or discarded",
            )));
        };
        fs::rename(&temp, &self.dest).map_err(|e| {
            // The rename failed, so the temp file is still there and no longer
            // owned by anything — unlink it now rather than leaving residue for
            // the next startup sweep.
            let _ = fs::remove_file(&temp);
            DropError::Storage(e)
        })?;
        sync_parent_dir(&self.dest);
        Ok(self.dest.clone())
    }

    /// Explicitly drop the image without publishing it. Equivalent to letting
    /// it fall out of scope; spelled out at refusal sites where the intent
    /// deserves to be visible.
    pub fn discard(self) {}
}

impl Drop for StoredImage {
    fn drop(&mut self) {
        if let Some(temp) = self.temp.take() {
            let _ = fs::remove_file(temp);
        }
    }
}

/// Cap on a drop caption, in chars — the `OTHER_MAX_CHARS` precedent. A caption
/// is a sentence or two typed on a phone; nothing legitimate approaches this.
///
/// Enforced in **two** places, deliberately. The route (U6) bounds the query
/// and refuses an over-cap caption with a 400 rather than silently delivering a
/// truncated one, because a caption clipped mid-sentence changes what the user
/// asked for. This cap is the composer's own backstop, applied through
/// [`crate::feed::io::clean`], so the composed prompt is bounded even if a
/// future caller forgets. In practice the route's refusal fires first.
pub const CAPTION_MAX_CHARS: usize = 512;

/// The fixed framing wrapped around a dropped image (R3).
///
/// The wording is not arbitrary: it must make the agent *open* the file, not
/// treat the path as a string to talk about — a bare path followed by a caption
/// reads ambiguously and would make the whole feature silently useless. This
/// exact phrasing was verified against a live bypass-permissions pane during the
/// U0 premise spike (Claude Code 2.1.219): the agent read the file and reported
/// its contents with no permission prompt. See
/// `docs/notes/2026-07-24-phone-drop-live-check.md`.
///
/// `{path}` is fly-minted (see [`mint_filename`]) and contains no whitespace, so
/// it cannot be split by the surrounding prose.
const PROMPT_FRAMING: &str = "Read the image at {path} — it's a screenshot I dropped from my phone.";

/// Compose the text delivered to the pane: the framing naming the stored image,
/// then the caption if there is one (R1, R3).
///
/// The caption is untrusted text bound for a PTY, so it goes through the
/// existing sanitize → scrub → truncate pipeline
/// ([`crate::feed::io::clean`]) *in that order* before composition — scrubbing
/// before sanitizing would let a zero-width char inside a token-shaped string
/// defeat the prefix match. `clean` returns `None` for a blank result, so a
/// whitespace-only caption is indistinguishable from no caption at all (AE3).
///
/// The result is handed to [`crate::feed::io::paste_payload`], which strips the
/// remaining control characters including ESC — so a caption cannot forge paste
/// markers regardless of what survives here.
pub fn compose_drop_prompt(path: &Path, caption: Option<&str>) -> String {
    let framed = PROMPT_FRAMING.replace("{path}", &path.display().to_string());
    match caption.and_then(|c| crate::feed::io::clean(c, CAPTION_MAX_CHARS)) {
        Some(c) => format!("{framed}\n\n{c}"),
        None => framed,
    }
}

/// Fill 8 bytes from the thread CSPRNG (the `hooks::token` / `config` idiom).
fn random_bytes() -> [u8; 8] {
    use rand::RngCore;
    let mut raw = [0u8; 8];
    rand::thread_rng().fill_bytes(&mut raw);
    raw
}

/// Read until `buf` is full or the reader ends, returning how many bytes
/// landed. A single `read` may return fewer bytes than asked for even when more
/// are coming, so the sniff must loop or it will abstain on a slow socket.
fn read_up_to(reader: &mut impl Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}

/// Remove fly-prefixed temp files older than [`TEMP_STALE_AFTER`] (KTD7).
///
/// The unlink-on-drop above covers early returns, not SIGKILL, a panic-abort, or
/// the user quitting fly mid-upload. In those a partial temp survives in a
/// directory the user browses, so it gets cleaned at next start. Entirely
/// best-effort — a sweep failure must never keep the store from constructing.
///
/// Only the fly prefix is ever considered: this directory belongs to the user,
/// and a sweep that deleted anything else would be a data-loss bug.
fn sweep_stale_temps(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.starts_with(TEMP_PREFIX) {
            continue;
        }
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|m| now.duration_since(m).ok())
            .is_some_and(|age| age > TEMP_STALE_AFTER);
        if stale {
            let _ = fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn home() -> PathBuf {
        PathBuf::from("/home/tester")
    }

    /// Fixtures live in `src-tauri/tests/fixtures/images/`. PNG/JPEG/GIF/WebP/
    /// AVIF are genuine encoder output (ffmpeg); the HEIC fixture is a
    /// hand-built ISO-BMFF `ftyp` box, because no encoder on this machine muxes
    /// HEIC — its first 16 bytes are byte-identical to a real HEIC's, which is
    /// the whole of what the sniffer reads.
    fn fixture(name: &str) -> Vec<u8> {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/images")
            .join(name);
        fs::read(&path).unwrap_or_else(|e| panic!("fixture {}: {e}", path.display()))
    }

    #[test]
    fn tilde_slash_expands_against_home() {
        let got = expand_tilde("~/projects/inbox", Some(&home())).unwrap();
        assert_eq!(got, PathBuf::from("/home/tester/projects/inbox"));
    }

    #[test]
    fn bare_tilde_is_home() {
        assert_eq!(expand_tilde("~", Some(&home())).unwrap(), home());
        // `~/` is the same place — a trailing slash is not a subdirectory.
        assert_eq!(expand_tilde("~/", Some(&home())).unwrap(), home());
    }

    #[test]
    fn absolute_path_passes_through_unchanged() {
        let got = expand_tilde("/srv/shots", Some(&home())).unwrap();
        assert_eq!(got, PathBuf::from("/srv/shots"));
    }

    #[test]
    fn absolute_path_does_not_need_home() {
        assert!(expand_tilde("/srv/shots", None).is_ok());
    }

    /// The case the whole helper exists for: a bare relative path must be
    /// refused, never resolved against the process cwd (`/` for a GUI launched
    /// from a desktop file).
    #[test]
    fn bare_relative_path_is_rejected() {
        assert_eq!(expand_tilde("inbox", Some(&home())), Err(DropDirError::Relative));
        assert_eq!(
            expand_tilde("./inbox", Some(&home())),
            Err(DropDirError::Relative)
        );
        assert_eq!(
            expand_tilde("../inbox", Some(&home())),
            Err(DropDirError::Relative)
        );
    }

    #[test]
    fn empty_and_whitespace_are_rejected() {
        assert_eq!(expand_tilde("", Some(&home())), Err(DropDirError::Empty));
        assert_eq!(expand_tilde("   ", Some(&home())), Err(DropDirError::Empty));
    }

    #[test]
    fn surrounding_whitespace_is_trimmed() {
        let got = expand_tilde("  ~/inbox  ", Some(&home())).unwrap();
        assert_eq!(got, PathBuf::from("/home/tester/inbox"));
    }

    #[test]
    fn foreign_home_is_rejected_not_guessed() {
        assert_eq!(
            expand_tilde("~root/inbox", Some(&home())),
            Err(DropDirError::ForeignHome)
        );
    }

    #[test]
    fn tilde_without_home_is_rejected() {
        assert_eq!(expand_tilde("~/inbox", None), Err(DropDirError::NoHome));
        assert_eq!(expand_tilde("~", None), Err(DropDirError::NoHome));
    }

    #[test]
    fn unconfigured_defaults_under_the_data_root() {
        let got = resolve_drop_dir(None, Some(&home()), Path::new("/data/fly")).unwrap();
        assert_eq!(got, PathBuf::from("/data/fly/inbox"));
    }

    #[test]
    fn configured_value_wins_over_the_data_root() {
        let got =
            resolve_drop_dir(Some("~/projects/inbox"), Some(&home()), Path::new("/data/fly"))
                .unwrap();
        assert_eq!(got, PathBuf::from("/home/tester/projects/inbox"));
    }

    #[test]
    fn configured_bad_shape_propagates_rather_than_falling_back() {
        let got = resolve_drop_dir(Some("inbox"), Some(&home()), Path::new("/data/fly"));
        assert_eq!(got, Err(DropDirError::Relative));
    }

    // ---- U2: sniffing ----

    #[test]
    fn every_supported_format_sniffs_from_a_real_fixture() {
        for (file, want) in [
            ("sample.png", ImageKind::Png),
            ("sample.jpg", ImageKind::Jpeg),
            ("sample.gif", ImageKind::Gif),
            ("sample.webp", ImageKind::Webp),
            ("sample.heic", ImageKind::Heic),
            ("sample.avif", ImageKind::Avif),
        ] {
            assert_eq!(sniff_image(&fixture(file)), Some(want), "{file}");
        }
    }

    #[test]
    fn non_images_abstain_rather_than_guessing() {
        assert_eq!(sniff_image(b""), None, "empty");
        assert_eq!(sniff_image(b"\x89PN"), None, "3-byte truncated PNG prefix");
        assert_eq!(sniff_image(&fixture("notimage.txt")), None, "text");
        assert_eq!(sniff_image(&fixture("notimage.pdf")), None, "pdf");
    }

    /// The near-miss the WHATWG pattern exists for: RIFF is a container family,
    /// so matching the `RIFF` prefix alone would accept a WAV as a WebP.
    #[test]
    fn riff_that_is_not_webp_abstains() {
        let wav = fixture("riff-not-webp.wav");
        assert!(wav.starts_with(b"RIFF"), "fixture really is RIFF");
        assert_eq!(sniff_image(&wav), None);
    }

    /// An ISO-BMFF file whose `ftyp` brand is neither HEIC nor AVIF abstains
    /// rather than being stored under a guessed extension.
    #[test]
    fn unknown_ftyp_brand_abstains() {
        let mut mp4 = Vec::from(&b"\x00\x00\x00\x20ftypisom"[..]);
        mp4.extend_from_slice(&[0u8; 8]);
        assert_eq!(sniff_image(&mp4), None);
    }

    /// R16 in one assertion: the bytes decide, not the declared content type.
    #[test]
    fn jpeg_bytes_win_over_a_png_content_type_claim() {
        // The route never consults the declared type at all, so the strongest
        // form of this test is that JPEG bytes yield a `.jpg` extension.
        let kind = sniff_image(&fixture("sample.jpg")).unwrap();
        assert_eq!(kind.extension(), "jpg");
    }

    // ---- U2: filename minting ----

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn mint_filename_is_deterministic_under_fixed_inputs() {
        let name = mint_filename(
            at("2026-07-25T00:31:07Z"),
            [0xde, 0xad, 0xbe, 0xef, 0x01, 0x02, 0x03, 0x04],
            ImageKind::Png,
        );
        assert_eq!(name, "20260725T003107Z-deadbeef01020304.png");
    }

    #[test]
    fn mint_filename_differs_on_different_random_bytes() {
        let now = at("2026-07-25T00:31:07Z");
        let a = mint_filename(now, [0; 8], ImageKind::Png);
        let b = mint_filename(now, [1; 8], ImageKind::Png);
        assert_ne!(a, b, "same instant, different randomness");
    }

    /// The invariant that lets the minted name be pasted into a prompt and used
    /// as a path component without quoting or sanitizing (R16, U3).
    #[test]
    fn minted_name_has_no_separator_traversal_or_whitespace() {
        for kind in [
            ImageKind::Png,
            ImageKind::Jpeg,
            ImageKind::Gif,
            ImageKind::Webp,
            ImageKind::Heic,
            ImageKind::Avif,
        ] {
            let name = mint_filename(at("2026-07-25T00:31:07Z"), [0xab; 8], kind);
            assert!(!name.contains('/'), "{name}");
            assert!(!name.contains(".."), "{name}");
            assert!(!name.contains(char::is_whitespace), "{name}");
            assert!(name.ends_with(kind.extension()), "{name}");
        }
    }

    // ---- U2: storage ----

    fn store_of(dir: &Path) -> DropStore {
        DropStore::new(dir).expect("store constructs")
    }

    /// Files left in the directory, excluding fly's own temp files.
    fn published(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| !n.starts_with(TEMP_PREFIX))
            .collect();
        v.sort();
        v
    }

    /// Every file, temp included — the leak check.
    fn all_entries(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn store_then_commit_publishes_one_file() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_of(tmp.path());
        let bytes = fixture("sample.png");
        let img = store.store(&mut bytes.as_slice(), 1 << 20).unwrap();
        assert_eq!(img.kind(), ImageKind::Png);
        let path = img.commit().unwrap();
        assert_eq!(fs::read(&path).unwrap(), bytes, "content round-trips");
        assert_eq!(published(tmp.path()).len(), 1);
        assert_eq!(all_entries(tmp.path()).len(), 1, "no temp residue");
    }

    #[test]
    fn commit_writes_0600_into_a_0700_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_of(tmp.path());
        let img = store
            .store(&mut fixture("sample.png").as_slice(), 1 << 20)
            .unwrap();
        let path = img.commit().unwrap();
        let fmode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        let dmode = fs::metadata(store.dir()).unwrap().permissions().mode() & 0o777;
        assert_eq!(fmode, 0o600, "file is owner-only");
        assert_eq!(dmode, 0o700, "directory is owner-only");
    }

    #[test]
    fn stored_path_is_inside_the_configured_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_of(tmp.path());
        let img = store
            .store(&mut fixture("sample.png").as_slice(), 1 << 20)
            .unwrap();
        let path = img.commit().unwrap();
        let canonical = fs::canonicalize(&path).unwrap();
        assert!(canonical.starts_with(store.dir()), "{canonical:?}");
    }

    /// A body exactly at the cap is accepted — the boundary is inclusive.
    #[test]
    fn body_exactly_at_the_cap_succeeds() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_of(tmp.path());
        let bytes = fixture("sample.png");
        let img = store
            .store(&mut bytes.as_slice(), bytes.len() as u64)
            .unwrap();
        img.commit().unwrap();
        assert_eq!(published(tmp.path()).len(), 1);
    }

    /// One byte over is refused, and nothing survives — not even a temp file.
    #[test]
    fn body_one_byte_over_the_cap_is_refused_and_leaves_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_of(tmp.path());
        let bytes = fixture("sample.png");
        let cap = bytes.len() as u64 - 1;
        let err = store.store(&mut bytes.as_slice(), cap).unwrap_err();
        assert!(matches!(err, DropError::Oversize), "{err:?}");
        assert!(all_entries(tmp.path()).is_empty(), "no residue");
    }

    /// Oversize is detected without buffering the whole body: the reader is
    /// bounded at cap+1, so a body far larger than the cap stops early.
    #[test]
    fn oversize_stops_reading_shortly_past_the_cap() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_of(tmp.path());
        let mut body = fixture("sample.png");
        body.resize(4 * 1024 * 1024, 0);
        let mut counting = CountingReader {
            inner: body.as_slice(),
            read: 0,
        };
        let err = store.store(&mut counting, 64 * 1024).unwrap_err();
        assert!(matches!(err, DropError::Oversize));
        assert!(
            counting.read < 4 * 1024 * 1024,
            "stopped early, read {} of 4 MiB",
            counting.read
        );
        assert!(all_entries(tmp.path()).is_empty());
    }

    #[test]
    fn non_image_body_is_refused_before_any_file_is_created() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_of(tmp.path());
        let err = store
            .store(&mut fixture("notimage.pdf").as_slice(), 1 << 20)
            .unwrap_err();
        assert!(matches!(err, DropError::BadFormat), "{err:?}");
        assert!(all_entries(tmp.path()).is_empty(), "nothing was created");
    }

    /// The invariant behind every refusal path in the route: an uncommitted
    /// handle cleans up after itself, so a caller's early return cannot orphan
    /// a partial image in a directory the user browses.
    #[test]
    fn dropping_an_uncommitted_image_unlinks_its_temp_file() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_of(tmp.path());
        {
            let img = store
                .store(&mut fixture("sample.png").as_slice(), 1 << 20)
                .unwrap();
            assert_eq!(all_entries(tmp.path()).len(), 1, "temp exists while held");
            assert!(all_entries(tmp.path())[0].starts_with(TEMP_PREFIX));
            drop(img);
        }
        assert!(all_entries(tmp.path()).is_empty(), "unlinked on drop");
    }

    #[test]
    fn explicit_discard_also_unlinks() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_of(tmp.path());
        let img = store
            .store(&mut fixture("sample.png").as_slice(), 1 << 20)
            .unwrap();
        img.discard();
        assert!(all_entries(tmp.path()).is_empty());
    }

    #[test]
    fn storing_into_a_read_only_directory_errors_rather_than_panicking() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_of(tmp.path());
        fs::set_permissions(tmp.path(), fs::Permissions::from_mode(0o500)).unwrap();
        let got = store.store(&mut fixture("sample.png").as_slice(), 1 << 20);
        // Restore before the assert so the TempDir can always clean up.
        fs::set_permissions(tmp.path(), fs::Permissions::from_mode(0o700)).unwrap();
        assert!(matches!(got, Err(DropError::Storage(_))), "expected storage error");
    }

    /// A slow reader that hands back one byte at a time must still sniff — the
    /// header read loops rather than trusting a single `read` to fill it.
    #[test]
    fn sniff_survives_a_reader_that_dribbles_one_byte_at_a_time() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_of(tmp.path());
        let bytes = fixture("sample.png");
        let mut dribble = DribbleReader {
            inner: bytes.as_slice(),
        };
        let img = store.store(&mut dribble, 1 << 20).unwrap();
        assert_eq!(img.kind(), ImageKind::Png);
        let path = img.commit().unwrap();
        assert_eq!(fs::read(path).unwrap(), bytes);
    }

    // ---- U2: startup sweep (KTD7) ----

    #[test]
    fn construction_sweeps_stale_temps_but_spares_user_files_and_fresh_temps() {
        let tmp = tempfile::tempdir().unwrap();
        let stale = tmp.path().join(format!("{TEMP_PREFIX}0000000000000000"));
        let fresh = tmp.path().join(format!("{TEMP_PREFIX}1111111111111111"));
        let mine = tmp.path().join("holiday-photo.png");
        for p in [&stale, &fresh, &mine] {
            fs::write(p, b"x").unwrap();
        }
        // Age only the stale one past the sweep threshold.
        let old = SystemTime::now() - TEMP_STALE_AFTER - Duration::from_secs(60);
        set_mtime(&stale, old);

        let _store = store_of(tmp.path());

        assert!(!stale.exists(), "stale fly temp was swept");
        assert!(fresh.exists(), "a fresh temp may be an upload in flight");
        assert!(
            mine.exists(),
            "the sweep must never touch a file that is not fly's — this is the \
             user's own directory (KTD4)"
        );
    }

    fn set_mtime(path: &Path, when: SystemTime) {
        let ft = filetime_from(when);
        let times = [ft, ft];
        let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: `c` is a valid NUL-terminated path and `times` is a 2-element
        // timeval array, which is exactly what `utimes(2)` expects.
        let rc = unsafe { libc::utimes(c.as_ptr(), times.as_ptr()) };
        assert_eq!(rc, 0, "utimes failed for {path:?}");
    }

    fn filetime_from(when: SystemTime) -> libc::timeval {
        let secs = when
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        libc::timeval {
            tv_sec: secs as libc::time_t,
            tv_usec: 0,
        }
    }

    // ---- U3: prompt composition ----

    fn prompt(caption: Option<&str>) -> String {
        compose_drop_prompt(
            Path::new("/home/tester/inbox/20260725T003107Z-deadbeef01020304.png"),
            caption,
        )
    }

    #[test]
    fn prompt_names_the_path_as_an_image_to_read_and_carries_the_caption() {
        let out = prompt(Some("the login button overlaps the header on iOS"));
        assert!(out.contains("20260725T003107Z-deadbeef01020304.png"), "{out}");
        assert!(
            out.contains("the login button overlaps the header on iOS"),
            "{out}"
        );
        // The framing must direct the agent to *open* the file, not merely
        // mention a path — this is the whole feature (R3, verified in U0).
        assert!(out.starts_with("Read the image at "), "{out}");
    }

    #[test]
    fn prompt_without_a_caption_is_the_framing_alone() {
        let out = prompt(None);
        assert!(out.starts_with("Read the image at "), "{out}");
        assert!(!out.trim().is_empty());
        assert!(!out.contains("\n\n"), "no dangling blank line: {out:?}");
    }

    /// AE3: a caption of only whitespace must be treated as no caption, not as
    /// an empty line the agent has to interpret.
    #[test]
    fn whitespace_only_caption_matches_the_no_caption_case() {
        assert_eq!(prompt(Some("   \n\t  ")), prompt(None));
        assert_eq!(prompt(Some("")), prompt(None));
    }

    /// The caption reaches a PTY, so no ESC may survive — otherwise it could
    /// forge the bracketed-paste end marker or smuggle a terminal sequence.
    #[test]
    fn caption_escape_bytes_do_not_survive() {
        let out = prompt(Some("before\x1b[201~after\x07end"));
        assert!(!out.contains('\x1b'), "{out:?}");
        assert!(!out.contains('\x07'), "{out:?}");
        assert!(out.contains("beforeafter") || out.contains("before"), "{out:?}");
    }

    #[test]
    fn caption_secrets_are_scrubbed() {
        let out = prompt(Some("token is sk-ant-api03-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"));
        assert!(
            !out.contains("sk-ant-api03-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"),
            "{out}"
        );
    }

    /// Truncation runs *after* scrubbing, so a secret straddling the cap is
    /// masked before its tail is cut (the `io::clean` ordering contract).
    #[test]
    fn overlong_caption_is_truncated_after_scrubbing() {
        let long = "x".repeat(CAPTION_MAX_CHARS + 500);
        let out = prompt(Some(&long));
        assert!(out.contains('…'), "truncation marker present: {out:?}");
        assert!(out.chars().count() < long.chars().count());

        // A secret placed right at the boundary is scrubbed, not half-cut.
        let mut straddle = "y".repeat(CAPTION_MAX_CHARS - 10);
        straddle.push_str("sk-ant-api03-BBBBBBBBBBBBBBBBBBBBBBBBBBBBBB");
        let out = prompt(Some(&straddle));
        assert!(!out.contains("sk-ant-api03-BBBB"), "{out}");
    }

    /// Bracketed paste exists precisely so a multi-line caption lands as one
    /// composer message — newlines must survive composition.
    #[test]
    fn multiline_caption_keeps_its_newlines() {
        let out = prompt(Some("line one\nline two"));
        assert!(out.contains("line one\nline two"), "{out:?}");
    }

    /// The path is emitted unquoted, so a minted name containing whitespace
    /// would be split by the surrounding prose. `mint_filename` guarantees it
    /// cannot; this pins the dependency between the two.
    #[test]
    fn composed_path_is_a_single_whitespace_free_token() {
        let name = mint_filename(at("2026-07-25T00:31:07Z"), [0x5a; 8], ImageKind::Png);
        let out = compose_drop_prompt(&PathBuf::from("/home/tester/inbox").join(&name), None);
        let found = out
            .split_whitespace()
            .find(|t| t.contains(&name))
            .expect("the path appears as one token");
        assert!(found.ends_with(&name), "{found}");
    }

    // ---- test readers ----

    /// Counts how many bytes were actually pulled, so the oversize path can
    /// assert it stopped early instead of draining the whole body.
    struct CountingReader<'a> {
        inner: &'a [u8],
        read: usize,
    }

    impl Read for CountingReader<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let n = self.inner.read(buf)?;
            self.read += n;
            Ok(n)
        }
    }

    /// Returns at most one byte per call — the shape a slow socket presents.
    struct DribbleReader<'a> {
        inner: &'a [u8],
    }

    impl Read for DribbleReader<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if buf.is_empty() {
                return Ok(0);
            }
            self.inner.read(&mut buf[..1])
        }
    }
}
