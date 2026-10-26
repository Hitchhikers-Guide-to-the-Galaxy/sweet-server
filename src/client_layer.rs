//! The hitchhiker client layer on sweet-served sites.
//!
//! On the node farms our security modules (wiki-security-hitchhiker,
//! wiki-security-passportjs-gated) end their client security.js with a loader
//! that adds plugin client scripts to every page: that is how the factory
//! chooser (List and Rollover menu styles) reaches every site. Sweet serves the
//! stock wiki-security-social / friends client, which has no such loader, so
//! the styles chosen with wiki-plugin-new were saved but never drawn.
//!
//! `bootstrap` returns the same loader, listing only plugins this vendor tree
//! actually has, so a missing plugin never 404s on every page.
//!
//! wiki-ux.js (hitchhiker's page-flag tools) is left out on purpose: sweet has
//! its own flag click, and the two would fight.

use std::path::Path;

/// (script element id, plugin name, client file)
const LAYER: &[(&str, &str, &str)] = &[
    ("wiki-chooser-loader", "chooser", "wiki-chooser.js"), // the factory chooser
];

/// The loader to append to /security/security.js, or "" when none of the
/// layer's plugins are installed.
pub fn bootstrap(vendor: &Path) -> String {
    let present: Vec<(&str, String)> = LAYER
        .iter()
        .filter(|(_, name, file)| {
            vendor
                .join(format!("wiki-plugin-{name}/client/{file}"))
                .is_file()
        })
        .map(|(id, name, file)| (*id, format!("/plugins/{name}/{file}")))
        .collect();
    if present.is_empty() {
        return String::new();
    }
    let list = present
        .iter()
        .map(|(id, src)| format!("['{id}', '{src}']"))
        .collect::<Vec<_>>()
        .join(",\n    ");
    format!(
        r#"
/* sweet-server: the hitchhiker client layer, as the node security modules load it. */
;(function () {{
  var bust = /localhost$/.test(location.hostname) ? '?v=' + Date.now() : ''
  var scripts = [
    {list}
  ]
  for (var i = 0; i < scripts.length; i++) {{
    try {{
      if (document.getElementById(scripts[i][0])) continue
      var s = document.createElement('script')
      s.id = scripts[i][0]
      s.src = scripts[i][1] + bust
      s.defer = true
      ;(document.head || document.documentElement).appendChild(s)
    }} catch (e) {{
      if (window.console) console.error('[sweet-server] client layer failed for ' + scripts[i][1], e)
    }}
  }}
}})();
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("sweet-client-layer-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn lists_the_chooser_when_installed() {
        let v = tmp("with");
        fs::create_dir_all(v.join("wiki-plugin-chooser/client")).unwrap();
        fs::write(v.join("wiki-plugin-chooser/client/wiki-chooser.js"), "x").unwrap();
        let js = bootstrap(&v);
        assert!(js.contains("['wiki-chooser-loader', '/plugins/chooser/wiki-chooser.js']"));
        assert!(!js.contains("wiki-ux"));
    }

    #[test]
    fn empty_when_nothing_installed() {
        assert_eq!(bootstrap(&tmp("without")), "");
    }
}
