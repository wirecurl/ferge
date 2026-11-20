//! Budget verdicts and their presentation.

use std::fmt::Write as _;

/// Outcome of one budget check.
#[derive(Clone, Debug)]
pub(crate) struct Verdict {
    pub(crate) budget: &'static str,
    pub(crate) target: String,
    pub(crate) measured: String,
    pub(crate) met: bool,
}

/// Renders verdicts as an aligned plain-text table.
pub(crate) fn table(verdicts: &[Verdict]) -> String {
    let headers = ["Budget", "Target", "Measured", "Status"];
    let rows: Vec<[String; 4]> = verdicts
        .iter()
        .map(|verdict| {
            [
                verdict.budget.to_owned(),
                verdict.target.clone(),
                verdict.measured.clone(),
                if verdict.met { "met" } else { "MISSED" }.to_owned(),
            ]
        })
        .collect();
    let mut widths = headers.map(|header| header.chars().count());
    for row in &rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    let mut out = String::new();
    let line = |out: &mut String, cells: [&str; 4]| {
        for (index, (cell, width)) in cells.iter().zip(widths).enumerate() {
            if index > 0 {
                out.push_str(" | ");
            }
            let _ = write!(out, "{cell:<width$}");
        }
        out.push('\n');
    };
    line(&mut out, headers);
    let rule: Vec<String> = widths.iter().map(|width| "-".repeat(*width)).collect();
    line(&mut out, [&rule[0], &rule[1], &rule[2], &rule[3]]);
    for row in &rows {
        line(&mut out, [&row[0], &row[1], &row[2], &row[3]]);
    }
    out
}
