//! The blog's web faces, bundled.
//!
//! The type scale names three families — IBM Plex Sans (body), IBM Plex Mono (numbers and
//! code), Newsreader (display serif). They used to arrive from the Google Fonts CDN, a runtime
//! fetch to a third party on every first paint. Here the woff2 ride in the binary (`include_bytes!`),
//! so a fresh clone serves its own type with cargo and nothing else — the same self-contained law
//! that brings Binaryen in as a crate rather than a machine install.
//!
//! The faces are the OFL-1.1 latin subsets from the `@fontsource` packages (see `fonts/OFL-*.txt`);
//! only the weights the scale actually asks for are carried. [`FACES`] is the single source: the
//! bytes it embeds and the `@font-face` rules [`install`] writes are generated from the same list,
//! so a face is declared once.

use std::fmt::Write as _;
use std::io;
use std::path::Path;

/// One bundled face: the family it answers to, its weight, the basename it serves under, and the
/// embedded woff2. `style` is always `normal` — the scale never asks for an italic face file
/// (it synthesises emphasis), so it is not a field.
pub struct Face {
    pub family: &'static str,
    pub weight: u16,
    pub file: &'static str,
    pub bytes: &'static [u8],
}

macro_rules! face {
    ($family:literal, $weight:literal, $file:literal) => {
        Face {
            family: $family,
            weight: $weight,
            file: concat!($file, ".woff2"),
            bytes: include_bytes!(concat!("../fonts/", $file, ".woff2")),
        }
    };
}

/// Every bundled face, in the order the scale introduces them. The single source of truth for
/// both the served bytes and the generated stylesheet.
pub const FACES: &[Face] = &[
    face!("IBM Plex Sans", 400, "ibm-plex-sans-400"),
    face!("IBM Plex Sans", 500, "ibm-plex-sans-500"),
    face!("IBM Plex Sans", 600, "ibm-plex-sans-600"),
    face!("IBM Plex Mono", 400, "ibm-plex-mono-400"),
    face!("IBM Plex Mono", 500, "ibm-plex-mono-500"),
    face!("IBM Plex Mono", 600, "ibm-plex-mono-600"),
    face!("Newsreader", 400, "newsreader-400"),
    face!("Newsreader", 500, "newsreader-500"),
];

/// Materialise the bundled fonts into `static_dir`: each woff2 under `<static_dir>/fonts/`, and a
/// `<static_dir>/fonts.css` of `@font-face` rules pointing at them under `/static/fonts/`. The
/// server calls this at boot so its static handler serves the type from the binary's own bytes.
/// Idempotent — a re-run rewrites the same files.
pub fn install(static_dir: &Path) -> io::Result<()> {
    let fonts = static_dir.join("fonts");
    std::fs::create_dir_all(&fonts)?;
    for (name, bytes) in [
        ("OFL-IBM-Plex.txt", include_bytes!("../fonts/OFL-IBM-Plex.txt").as_slice()),
        ("OFL-Newsreader.txt", include_bytes!("../fonts/OFL-Newsreader.txt").as_slice()),
    ] {
        std::fs::write(fonts.join(name), bytes)?;
    }
    let mut css = String::new();
    for face in FACES {
        std::fs::write(fonts.join(face.file), face.bytes)?;
        let _ = write!(
            css,
            "@font-face{{font-family:'{}';font-style:normal;font-weight:{};\
             font-display:swap;src:url('/static/fonts/{}') format('woff2')}}\n",
            face.family, face.weight, face.file,
        );
    }
    std::fs::write(static_dir.join("fonts.css"), css)
}
