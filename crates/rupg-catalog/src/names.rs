//! The names that PostgreSQL makes for objects that a statement does not name, as `makeObjectName`, `ChooseIndexNameAddition` and `ChooseIndexColumnNames` of `indexcmds.c` make them.

/// The size of a `name` value with its terminating zero byte. A name has at most `NAMEDATALEN - 1` bytes.
pub const NAMEDATALEN: usize = 64;

/// The longest prefix of `name` that has at most `len` bytes and ends at a character boundary, as `pg_mbcliplen` gives it.
pub fn clip(name: &str, len: usize) -> &str {
    let mut end = len.min(name.len());
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    &name[..end]
}

/// `makeObjectName`: `name1_name2_label`, with `name2` and `label` only when they are given.
///
/// When the result is longer than `NAMEDATALEN - 1` bytes, the longer of the two names loses one byte at a time until the result fits. The label is never shorter.
pub fn make_object_name(name1: &str, name2: Option<&str>, label: Option<&str>) -> String {
    let mut overhead = 0;
    let mut name1_len = name1.len();
    let mut name2_len = 0;
    if let Some(name2) = name2 {
        name2_len = name2.len();
        overhead += 1;
    }
    if let Some(label) = label {
        overhead += label.len() + 1;
    }
    let available = (NAMEDATALEN - 1).saturating_sub(overhead);
    while name1_len + name2_len > available {
        if name1_len > name2_len {
            name1_len -= 1;
        } else {
            name2_len -= 1;
        }
    }
    let mut name = clip(name1, name1_len).to_string();
    if let Some(name2) = name2 {
        name.push('_');
        name.push_str(clip(name2, name2_len));
    }
    if let Some(label) = label {
        name.push('_');
        name.push_str(label);
    }
    name
}

/// `ChooseIndexNameAddition` and `ChooseForeignKeyConstraintNameAddition`: the column names joined with `_`. The join stops after the name that makes it `NAMEDATALEN` bytes or longer.
pub fn name_addition<'a>(names: impl IntoIterator<Item = &'a str>) -> String {
    let mut addition = String::new();
    for name in names {
        if !addition.is_empty() {
            addition.push('_');
        }
        addition.push_str(clip(name, NAMEDATALEN - 1));
        if addition.len() >= NAMEDATALEN {
            break;
        }
    }
    addition
}

/// `ChooseIndexColumnNames`: the names of the columns of a new index. A name that an earlier column has gets the suffix 1, then 2, and so on, and the name is clipped so that the result has at most `NAMEDATALEN - 1` bytes.
///
/// Each input name is the name that the statement gives the index column, else the name of the table column, else `expr` for an expression.
pub fn index_column_names<'a>(names: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let mut result: Vec<String> = Vec::new();
    for original in names {
        let mut name = original.to_string();
        let mut suffix = 1;
        while result.contains(&name) {
            let digits = suffix.to_string();
            name = format!("{}{digits}", clip(original, NAMEDATALEN - 1 - digits.len()));
            suffix += 1;
        }
        result.push(name);
    }
    result
}
