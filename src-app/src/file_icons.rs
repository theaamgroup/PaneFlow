//! Per-language file icons, shared by the diff dock's file headers and the
//! Files sidebar tree.
//!
//! The assets under `icons/languages/` carry their own `fill`, so they must be
//! painted as images (`img()` / `ImgResourceLoader`), never through `svg()`,
//! which recolors the whole glyph with the element's text color.

/// Map a file's basename to its language icon asset path, or `None` for a
/// name no language claims.
///
/// This is the single icon policy for every surface (Files tree, diff body,
/// diff-dock tab strip); callers supply only their own unknown-file fallback
/// (issue #220). Full-name matches win over extensions (`Dockerfile`,
/// `Makefile`, `angular.json`), and the React Native platform suffixes are
/// checked before the plain `.ts`/`.js` families. There is no JavaScript
/// asset, so the `.js` family borrows the React glyph.
pub(crate) fn language_icon(basename: &str) -> Option<&'static str> {
    let basename = basename.trim().to_ascii_lowercase();
    match basename.as_str() {
        "angular.json" => return Some("icons/languages/angular.svg"),
        "dockerfile" | "containerfile" => return Some("icons/languages/docker.svg"),
        "makefile" => return Some("icons/languages/makefile.svg"),
        _ => {}
    }

    if matches!(
        basename.as_str(),
        name if name.ends_with(".native.js")
            || name.ends_with(".native.jsx")
            || name.ends_with(".native.ts")
            || name.ends_with(".native.tsx")
            || name.ends_with(".ios.js")
            || name.ends_with(".ios.jsx")
            || name.ends_with(".ios.ts")
            || name.ends_with(".ios.tsx")
            || name.ends_with(".android.js")
            || name.ends_with(".android.jsx")
            || name.ends_with(".android.ts")
            || name.ends_with(".android.tsx")
    ) {
        return Some("icons/languages/react-native.svg");
    }

    let ext = basename.rsplit('.').next().filter(|ext| *ext != basename)?;

    match ext {
        "css" | "less" | "sass" | "scss" => Some("icons/languages/css.svg"),
        "go" => Some("icons/languages/go.svg"),
        "apng" | "avif" | "bmp" | "gif" | "heic" | "heif" | "ico" | "jpe" | "jpeg" | "jpg"
        | "png" | "svg" | "tif" | "tiff" | "webp" => Some("icons/languages/image.svg"),
        "json" | "jsonc" => Some("icons/languages/json.svg"),
        "cjs" | "js" | "jsx" | "mjs" | "tsx" => Some("icons/languages/react.svg"),
        "log" => Some("icons/languages/log.svg"),
        "markdown" | "md" | "mdx" => Some("icons/languages/markdown.svg"),
        "py" | "pyi" | "pyw" => Some("icons/languages/python.svg"),
        "rb" | "rake" => Some("icons/languages/ruby.svg"),
        "rs" => Some("icons/languages/rust-small.svg"),
        "swift" => Some("icons/languages/swift.svg"),
        "text" | "txt" => Some("icons/languages/text.svg"),
        "toml" => Some("icons/languages/toml.svg"),
        "cts" | "mts" | "ts" => Some("icons/languages/typescript.svg"),
        _ => None,
    }
}

/// [`language_icon`] with the generic document as the unknown-file fallback,
/// as the Files tree and the diff body paint it.
pub(crate) fn language_icon_path(basename: &str) -> &'static str {
    language_icon(basename).unwrap_or("icons/languages/file.svg")
}

/// Cross-surface expectations shared by the `file_icons` tests and the diff
/// dock's `file_tab_icon` tests (issue #220). `None` is "unknown file": each
/// surface supplies its own fallback there and must agree everywhere else.
#[cfg(test)]
pub(crate) mod cases {
    pub(crate) const CASES: &[(&str, Option<&str>)] = &[
        // TSX/JSX are React on every surface; the tab strip used to say TypeScript.
        ("view.TSX", Some("icons/languages/react.svg")),
        ("Button.jsx", Some("icons/languages/react.svg")),
        // There is no javascript.svg: the JS family borrows React's glyph.
        ("app.js", Some("icons/languages/react.svg")),
        ("index.mjs", Some("icons/languages/react.svg")),
        ("config.cjs", Some("icons/languages/react.svg")),
        ("index.ts", Some("icons/languages/typescript.svg")),
        ("Button.ios.tsx", Some("icons/languages/react-native.svg")),
        ("Dockerfile", Some("icons/languages/docker.svg")),
        ("Containerfile", Some("icons/languages/docker.svg")),
        ("angular.json", Some("icons/languages/angular.svg")),
        ("Makefile", Some("icons/languages/makefile.svg")),
        ("logo.png", Some("icons/languages/image.svg")),
        ("photo.HEIC", Some("icons/languages/image.svg")),
        ("styles.css", Some("icons/languages/css.svg")),
        ("styles.scss", Some("icons/languages/css.svg")),
        ("tsconfig.jsonc", Some("icons/languages/json.svg")),
        ("notes.text", Some("icons/languages/text.svg")),
        ("tasks.rake", Some("icons/languages/ruby.svg")),
        ("script.pyw", Some("icons/languages/python.svg")),
        ("main.rs", Some("icons/languages/rust-small.svg")),
        ("LICENSE", None),
        ("notes.xyz", None),
        (".gitignore", None),
        ("", None),
    ];
}

#[cfg(test)]
mod tests {
    use super::{cases::CASES, language_icon, language_icon_path};

    /// Issue #220: the shared table is the policy; `language_icon_path` only
    /// adds the generic-document fallback on top of it.
    #[test]
    fn the_shared_table_is_the_language_policy() {
        for (name, expected) in CASES {
            assert_eq!(language_icon(name), *expected, "language_icon({name:?})");
            assert_eq!(
                language_icon_path(name),
                expected.unwrap_or("icons/languages/file.svg"),
                "language_icon_path({name:?})"
            );
        }
    }

    #[test]
    fn full_names_win_over_extensions() {
        assert_eq!(
            language_icon_path("Dockerfile"),
            "icons/languages/docker.svg"
        );
        assert_eq!(
            language_icon_path("angular.json"),
            "icons/languages/angular.svg"
        );
        assert_eq!(
            language_icon_path("Makefile"),
            "icons/languages/makefile.svg"
        );
    }

    #[test]
    fn platform_suffixes_win_over_the_base_extension() {
        assert_eq!(
            language_icon_path("Button.ios.tsx"),
            "icons/languages/react-native.svg"
        );
        assert_eq!(
            language_icon_path("Button.tsx"),
            "icons/languages/react.svg"
        );
    }

    #[test]
    fn dotfiles_and_unknown_names_fall_back() {
        // A leading-dot name is all extension, so it must not be read as one.
        assert_eq!(language_icon_path(".gitignore"), "icons/languages/file.svg");
        assert_eq!(language_icon_path("LICENSE"), "icons/languages/file.svg");
        assert_eq!(
            language_icon_path("main.rs"),
            "icons/languages/rust-small.svg"
        );
    }
}
