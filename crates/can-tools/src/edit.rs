//! Tool `edit_file`: ersetzt eine Textstelle in einer Datei, statt die Datei neu zu schreiben.

use can_core::{ToolCtx, ToolError};
use can_macros::tool;

use crate::fs::{read_text, resolve};

/// Ersetzt in einer Datei des Arbeitsverzeichnisses `old_text` durch `new_text`. Die Stelle muss
/// wörtlich und genau einmal vorkommen; mit `replace_all` werden alle Vorkommen ersetzt.
#[tool(effect = "write")]
pub async fn edit_file(
    cx: &ToolCtx,
    path: String,
    old_text: String,
    new_text: String,
    replace_all: Option<bool>,
) -> Result<String, ToolError> {
    let for_model = |message: String| ToolError::ForModel(message);
    if old_text.is_empty() {
        return Err(for_model("`old_text` ist leer.".to_owned()));
    }
    if old_text == new_text {
        return Err(for_model(
            "`old_text` und `new_text` sind gleich.".to_owned(),
        ));
    }
    let content = read_text(cx, &path).await?;
    let count = content.matches(&old_text).count();
    let all = replace_all.unwrap_or(false);
    match count {
        0 => {
            return Err(for_model(format!(
                "Die Stelle kommt in {path} nicht vor. Die Datei vorher lesen und den Text genau übernehmen."
            )));
        }
        1 => {}
        _ if all => {}
        _ => {
            return Err(for_model(format!(
                "Die Stelle kommt in {path} {count}-mal vor. Mehr umgebenden Text angeben oder `replace_all` setzen."
            )));
        }
    }
    let updated = if all {
        content.replace(&old_text, &new_text)
    } else {
        content.replacen(&old_text, &new_text, 1)
    };
    let resolved = resolve(&cx.workdir, &path)?;
    tokio::fs::write(&resolved, updated)
        .await
        .map_err(|error| for_model(format!("Datei nicht schreibbar: {path} ({error})")))?;
    Ok(format!(
        "{path}: {} ersetzt.",
        if count == 1 {
            "1 Stelle".to_owned()
        } else {
            format!("{count} Stellen")
        }
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::TempDir;
    use can_core::{Effect, Tool};
    use serde_json::{Value, json};

    async fn call(dir: &TempDir, args: Value) -> Result<String, String> {
        match edit_file.call(args, &dir.ctx()).await {
            Ok(output) => Ok(output.as_text()),
            Err(ToolError::ForModel(message)) => Err(message),
            Err(other) => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn replaces_one_unique_place_and_leaves_the_rest() {
        let dir = TempDir::new();
        dir.write("a.rs", b"fn alt() {}\nfn bleibt() {}\n");
        assert_eq!(
            (edit_file.spec().effect, edit_file.spec().idempotent),
            (Effect::Write, false)
        );
        let done = call(
            &dir,
            json!({"path": "a.rs", "old_text": "fn alt()", "new_text": "fn neu()"}),
        )
        .await
        .unwrap();
        assert_eq!(done, "a.rs: 1 Stelle ersetzt.");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.rs")).unwrap(),
            "fn neu() {}\nfn bleibt() {}\n"
        );
    }

    #[tokio::test]
    async fn ambiguous_or_missing_places_change_nothing() {
        let dir = TempDir::new();
        dir.write("a.txt", b"x = 1\nx = 1\n");
        let twice = call(
            &dir,
            json!({"path": "a.txt", "old_text": "x = 1", "new_text": "x = 2"}),
        )
        .await
        .unwrap_err();
        assert!(twice.contains("2-mal"), "{twice}");
        let missing = call(
            &dir,
            json!({"path": "a.txt", "old_text": "y", "new_text": "z"}),
        )
        .await
        .unwrap_err();
        assert!(missing.contains("kommt in a.txt nicht vor"));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "x = 1\nx = 1\n"
        );

        let all = call(
            &dir,
            json!({"path": "a.txt", "old_text": "x = 1", "new_text": "x = 2", "replace_all": true}),
        )
        .await;
        assert_eq!(all.unwrap(), "a.txt: 2 Stellen ersetzt.");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "x = 2\nx = 2\n"
        );
    }

    #[tokio::test]
    async fn bad_arguments_and_paths_outside_are_refused() {
        let dir = TempDir::new();
        dir.write("a.txt", b"inhalt");
        assert!(
            call(
                &dir,
                json!({"path": "a.txt", "old_text": "", "new_text": "x"})
            )
            .await
            .unwrap_err()
            .contains("leer")
        );
        assert!(
            call(
                &dir,
                json!({"path": "a.txt", "old_text": "x", "new_text": "x"})
            )
            .await
            .unwrap_err()
            .contains("gleich")
        );
        assert!(
            call(
                &dir,
                json!({"path": "../a.txt", "old_text": "a", "new_text": "b"})
            )
            .await
            .unwrap_err()
            .contains("außerhalb")
        );
        assert!(
            call(
                &dir,
                json!({"path": "fehlt.txt", "old_text": "a", "new_text": "b"})
            )
            .await
            .unwrap_err()
            .contains("nicht lesbar")
        );
    }
}
