#[derive(serde::Deserialize)]
struct ShaderSelection {
    name: String,
    enabled: bool,
}

/// The legacy control command uses bare names and reserves off/list. The
/// structured variant keeps filenames distinct from commands and disables
/// a pack only if that exact pack is still selected.
pub(super) fn change(
    command: &str,
    argument: &str,
    selected: &str,
    canonical: impl Fn(&str) -> String,
) -> Result<Option<String>, String> {
    if command == "shader-pack-set" {
        let ShaderSelection { name, enabled } =
            serde_json::from_str(argument).map_err(|error| error.to_string())?;
        // The graphics settings file stores its selection on one trimmed line.
        if name.is_empty() || name.trim() != name || name.contains(['\r', '\n', '\0']) {
            return Err("the shader pack filename cannot be saved".into());
        }
        let name = canonical(&name);
        return Ok(match (enabled, selected == name) {
            (true, _) => Some(name),
            (false, true) => Some(String::new()),
            _ => None,
        });
    }
    Ok(match argument {
        "" | "list" => None,
        "off" => Some(String::new()),
        name => Some(canonical(name)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_shader_filenames_resolve_before_conditional_selection() {
        let canonical = |name: &str| match name {
            "Example.hshader" => "horizon:example".into(),
            _ => name.to_owned(),
        };
        let on = r#"{"name":"Example.hshader","enabled":true}"#;
        let off = r#"{"name":"Example.hshader","enabled":false}"#;
        assert_eq!(
            change("shader-pack-set", on, "", canonical)
                .unwrap()
                .as_deref(),
            Some("horizon:example")
        );
        assert_eq!(
            change("shader-pack-set", on, "horizon:example", canonical)
                .unwrap()
                .as_deref(),
            Some("horizon:example")
        );
        assert_eq!(
            change("shader-pack-set", off, "horizon:other", canonical).unwrap(),
            None
        );
        assert_eq!(
            change("shader-pack-set", off, "horizon:example", canonical)
                .unwrap()
                .as_deref(),
            Some("")
        );
    }

    #[test]
    fn structured_names_are_not_interpreted_as_commands() {
        for name in ["off", "list", "A \"quoted\" shader.zip", "春 shader.zip"] {
            let argument = serde_json::json!({"name": name, "enabled": true}).to_string();
            assert_eq!(
                change("shader-pack-set", &argument, "old", str::to_owned)
                    .unwrap()
                    .as_deref(),
                Some(name)
            );
        }
        assert_eq!(
            change("shader-pack", "list", "old", str::to_owned).unwrap(),
            None
        );
        assert_eq!(
            change("shader-pack", "off", "old", str::to_owned).unwrap(),
            Some(String::new())
        );
    }

    #[test]
    fn disabling_is_conditional_and_unsavable_names_are_refused() {
        let argument = r#"{"name":"A.zip","enabled":false}"#;
        assert_eq!(
            change("shader-pack-set", argument, "B.zip", str::to_owned).unwrap(),
            None
        );
        assert_eq!(
            change("shader-pack-set", argument, "A.zip", str::to_owned).unwrap(),
            Some(String::new())
        );
        for name in ["", " A.zip", "A.zip ", "A\nB.zip"] {
            let invalid = serde_json::json!({"name": name, "enabled": true}).to_string();
            assert!(change("shader-pack-set", &invalid, "old", str::to_owned).is_err());
        }
    }
}
