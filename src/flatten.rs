//! Flat columns for `--flat` (spec "Flat columns").
//!
//! The separator is `__`. Question and field names that contain it are
//! rejected in templates ([`validate_names`]), so column names never collide:
//! every column of question `q` is `q` or `q__<suffix>`, and no suffix starts
//! with `_`.

use serde_json::{Map, Value};

/// Separator between a question or field name and a column suffix.
pub const SEP: &str = "__";

/// Column name `name__suffix`.
pub fn col(name: &str, suffix: &str) -> String {
    format!("{name}{SEP}{suffix}")
}

/// Column name `name__p__option`.
pub fn prob_col(name: &str, option: &str) -> String {
    format!("{name}{SEP}p{SEP}{option}")
}

/// Reject names that would make flat columns ambiguous.
pub fn validate_names<'a>(names: impl IntoIterator<Item = &'a str>) -> Result<(), String> {
    for n in names {
        if n.is_empty() {
            return Err("question and field names must not be empty".into());
        }
        if n.contains(SEP) {
            return Err(format!(
                "name `{n}` contains `{SEP}`, the --flat column separator; rename it"
            ));
        }
    }
    Ok(())
}

/// True if `column` is one of the flat columns of `name` (`name` or `name__*`).
pub fn is_column_of(column: &str, name: &str) -> bool {
    column == name
        || column
            .strip_prefix(name)
            .is_some_and(|rest| rest.starts_with(SEP))
}

/// Columns as a JSON object, in order.
pub fn to_object(cols: Vec<(String, Value)>) -> Map<String, Value> {
    cols.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_with_separator_are_rejected() {
        assert!(validate_names(["ok", "also_ok", "_x_"]).is_ok());
        let err = validate_names(["fine", "bad__name"]).unwrap_err();
        assert!(err.contains("bad__name"));
        assert!(validate_names([""]).is_err());
    }

    #[test]
    fn column_membership() {
        assert!(is_column_of("topic", "topic"));
        assert!(is_column_of("topic__conf", "topic"));
        assert!(is_column_of("topic__p__billing", "topic"));
        assert!(!is_column_of("topics", "topic"));
        assert!(!is_column_of("topic_conf", "topic"));
    }
}
