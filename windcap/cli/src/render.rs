//! Drawing tables and charts with nothing but ASCII, measured in terminal cells.
//!
//! Two constraints shape this module:
//!
//!   * **CJK text is two cells wide per glyph.** Padding by `char` count or byte count makes the
//!     columns drift apart exactly where the data is interesting, so every width here is in cells.
//!   * **Only ASCII glyphs are drawn.** A cp936 console passes `#`, `.` and `-` through untouched,
//!     so a chart keeps its shape even when the Chinese text beside it renders as `?`.
//!
//! Nothing here touches a terminal, the clock or the database: each function turns values into a
//! `String`, which is what makes the whole module testable without a screen.

use std::fmt::Write as _;

/// Cells a code point occupies. Wide/Fullwidth ranges come from UAX #11 East Asian Width; combining
/// marks and control characters occupy none.
fn char_width(ch: char) -> usize {
    let u = ch as u32;
    if matches!(u, 0x0300..=0x036F | 0x200B..=0x200F | 0xFE00..=0xFE0F) || u < 0x20 || u == 0x7F {
        return 0;
    }
    let wide = matches!(u,
        0x1100..=0x115F
            | 0x2E80..=0x303E
            | 0x3041..=0x33FF
            | 0x3400..=0x4DBF
            | 0x4E00..=0x9FFF
            | 0xA000..=0xA4CF
            | 0xAC00..=0xD7A3
            | 0xF900..=0xFAFF
            | 0xFE30..=0xFE6F
            | 0xFF00..=0xFF60
            | 0xFFE0..=0xFFE6
            | 0x1F300..=0x1F64F
            | 0x20000..=0x2FFFD);
    if wide {
        2
    } else {
        1
    }
}

/// Display width in terminal cells, not characters and not bytes.
pub fn display_width(text: &str) -> usize {
    text.chars().map(char_width).sum()
}

/// Collapse every run of whitespace to a single space and trim.
///
/// `ocr_text` is full of `\r\n` from the OCR engine, and one embedded newline in a table row breaks
/// the alignment of every row under it — so a body line is flattened before it is measured.
pub fn flatten(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_space = true;
    for ch in text.chars() {
        if ch.is_whitespace() {
            if !in_space {
                out.push(' ');
                in_space = true;
            }
            continue;
        }
        out.push(ch);
        in_space = false;
    }
    // A trailing space would be invisible here and visible to the column widths under it.
    while out.ends_with(' ') {
        out.pop();
    }
    out
}

/// Shorten to `cells` terminal cells, marking a real cut with `..`.
pub fn clip(text: &str, cells: usize) -> String {
    if display_width(text) <= cells {
        return text.to_string();
    }
    let budget = cells.saturating_sub(2);
    let mut out = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let width = char_width(ch);
        if used + width > budget {
            break;
        }
        used += width;
        out.push(ch);
    }
    out.push_str("..");
    out
}

/// Shorten to `count` characters, marking a real cut with `..`.
///
/// Counting characters rather than cells is right for the trailing body column, which is the one
/// column that is never padded: a Chinese body of sixty glyphs is what "the first sixty characters"
/// means to a reader, and this column's width cannot misalign anything under it.
pub fn clip_chars(text: &str, count: usize) -> String {
    match text.char_indices().nth(count) {
        None => text.to_string(),
        Some((at, _)) => format!("{}..", &text[..at]),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    Right,
}

/// Pad to `cells`. Content already wider than the column is left alone: silently truncating here
/// would hide data, and a caller who wants a cap should have reached for [`clip`] first.
fn pad(text: &str, cells: usize, align: Align) -> String {
    let width = display_width(text);
    if width >= cells {
        return text.to_string();
    }
    let gap = " ".repeat(cells - width);
    match align {
        Align::Left => format!("{text}{gap}"),
        Align::Right => format!("{gap}{text}"),
    }
}

/// An aligned text table. Widths come from the content, so rendering is a pure function of the rows
/// and every report stays byte-for-byte reproducible in a test.
pub struct Table {
    headers: Vec<String>,
    aligns: Vec<Align>,
    rows: Vec<Vec<String>>,
}

impl Table {
    pub fn new(headers: &[&str], aligns: &[Align]) -> Table {
        Table {
            headers: headers.iter().map(|h| h.to_string()).collect(),
            aligns: aligns.to_vec(),
            rows: Vec::new(),
        }
    }

    pub fn push(&mut self, cells: Vec<String>) -> &mut Table {
        self.rows.push(cells);
        self
    }

    fn widths(&self) -> Vec<usize> {
        let mut widths = vec![0usize; self.headers.len()];
        for (i, header) in self.headers.iter().enumerate() {
            widths[i] = widths[i].max(display_width(header));
        }
        for row in &self.rows {
            for (i, cell) in row.iter().enumerate().take(widths.len()) {
                widths[i] = widths[i].max(display_width(cell));
            }
        }
        widths
    }

    /// Header, dash rule, then one line per row. The final column is never padded, so no line ends
    /// in trailing spaces and a pasted report stays diffable.
    pub fn render(&self) -> String {
        let widths = self.widths();
        let line = |cells: &[String], fill: bool| -> String {
            let last = widths.len().saturating_sub(1);
            (0..widths.len())
                .map(|i| {
                    if fill {
                        "-".repeat(widths[i])
                    } else {
                        let cell = cells.get(i).map(String::as_str).unwrap_or("");
                        // A ragged right edge is not a misalignment, so the last column skips `pad`.
                        if i == last {
                            cell.to_string()
                        } else {
                            pad(cell, widths[i], self.aligns.get(i).copied().unwrap_or(Align::Left))
                        }
                    }
                })
                .collect::<Vec<_>>()
                .join("  ")
        };
        let mut out = String::new();
        let _ = writeln!(out, "{}", line(&self.headers, false));
        let _ = writeln!(out, "{}", line(&[], true));
        for row in &self.rows {
            let _ = out.write_str(&line(row, false));
            out.push('\n');
        }
        out.trim_end_matches('\n').to_string()
    }
}

/// Collapse `values` into `columns` groups by summation.
///
/// Summing rather than sampling keeps the total honest: a 240-bucket day squeezed into 72 columns
/// must still show a busy hour as a busy hour.
pub fn resample(values: &[usize], columns: usize) -> Vec<usize> {
    if columns == 0 {
        return Vec::new();
    }
    if values.len() <= columns {
        return values.to_vec();
    }
    (0..columns)
        .map(|i| {
            let low = i * values.len() / columns;
            let high = ((i + 1) * values.len() / columns).max(low + 1).min(values.len());
            values[low..high].iter().sum()
        })
        .collect()
}

/// A bottom-up column chart, returned top row first. `#` is filled, `.` is empty.
///
/// Any non-zero value gets at least one cell: on a day with three thousand captures at noon and one
/// at 23:00, the evening would otherwise be indistinguishable from no data at all.
pub fn column_chart(values: &[usize], height: usize) -> Vec<String> {
    let height = height.max(1);
    let peak = values.iter().copied().max().unwrap_or(0);
    let filled: Vec<usize> = values
        .iter()
        .map(|v| {
            if *v == 0 || peak == 0 {
                0
            } else {
                (v.saturating_mul(height) + peak - 1) / peak
            }
        })
        .collect();
    (0..height)
        .map(|row| {
            let need = height - row;
            filled.iter().map(|v| if *v >= need && *v > 0 { "#" } else { "." }).collect()
        })
        .collect()
}

/// A horizontal bar scaled to `max`, `#` filled and `.` empty.
pub fn bar(value: i64, max: i64, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let filled = if max <= 0 || value <= 0 {
        0
    } else {
        (((value as f64 / max as f64) * width as f64).round() as usize).clamp(1, width)
    };
    format!("{}{}", "#".repeat(filled), ".".repeat(width - filled))
}

/// A ruler of evenly spaced ticks plus the labels under them, as two equal-length lines.
///
/// Labels are centred on their tick and shifted back inside the line when they would overrun it, so
/// the last axis label stays readable instead of being chopped to one character.
pub fn ruler(width: usize, labels: &[String]) -> (String, String) {
    if labels.is_empty() || width == 0 {
        return (" ".repeat(width), " ".repeat(width));
    }
    let positions: Vec<usize> = if labels.len() == 1 {
        vec![0]
    } else {
        (0..labels.len()).map(|i| i * (width - 1) / (labels.len() - 1)).collect()
    };
    let mut ticks = vec!['-'; width];
    let mut text = vec![' '; width];
    for at in &positions {
        ticks[*at] = '|';
    }
    for (label, at) in labels.iter().zip(&positions) {
        let label = clip(label, 5);
        let len = label.chars().count();
        let start = (at.saturating_sub(len / 2)).min(width.saturating_sub(len));
        for (offset, ch) in label.chars().enumerate() {
            if let Some(slot) = text.get_mut(start + offset) {
                *slot = ch;
            }
        }
    }
    (ticks.into_iter().collect(), text.into_iter().collect())
}

/// Pick `count` labels spread over a longer list, for an axis under a resampled chart.
pub fn stride_labels(labels: &[String], count: usize) -> Vec<String> {
    if labels.is_empty() || count == 0 {
        return Vec::new();
    }
    if labels.len() <= count {
        return labels.to_vec();
    }
    (0..count).map(|i| labels[i * labels.len() / count].clone()).collect()
}

/// A seconds-into-segment offset as `+0:05:30`, keeping the sign the data actually has.
///
/// A row can precede its own segment's first second when a restarted segment keeps the original
/// filename, and rendering that as `+-0:00:03` reads like a bug in the tool.
pub fn offset(seconds: i64) -> String {
    let body = wind_base::clock::seconds_to_hhmmss(seconds);
    if seconds < 0 {
        body
    } else {
        format!("+{body}")
    }
}

/// A quoted value, or a named placeholder when it is empty.
///
/// "searched for nothing" and "searched for a blank string" have to be tellable apart in a report,
/// because the first one is what a whole-day scan looks like and the second is a user typo.
pub fn quoted_or(value: &str, absent: &str) -> String {
    if value.is_empty() {
        absent.to_string()
    } else {
        format!("'{value}'")
    }
}

/// Milliseconds in a fixed number of cells, so a column of timings lines up without a width pass.
pub fn millis(ms: f64) -> String {
    format!("{ms:>8.3} ms")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The cell offset of a substring, which is the only alignment claim worth testing.
    fn cell_of(line: &str, needle: &str) -> usize {
        display_width(&line[..line.find(needle).unwrap_or_else(|| panic!("'{needle}' not in {line:?}"))])
    }

    #[test]
    fn chinese_glyphs_count_as_two_cells_and_ascii_as_one() {
        assert_eq!(display_width("abc"), 3);
        assert_eq!(display_width("文件"), 4);
        assert_eq!(display_width("a文b"), 4);
        assert_eq!(display_width("，"), 2, "the full-width comma is Fullwidth, not narrow");
        assert_eq!(display_width("\u{300}"), 0, "a combining mark takes no cell");
        assert_eq!(display_width(""), 0);
    }

    #[test]
    fn flattening_kills_the_newlines_that_would_break_a_table() {
        assert_eq!(flatten("文件\r\nChatGPT\r\n新聊天"), "文件 ChatGPT 新聊天");
        assert_eq!(flatten("  leading and   trailing  "), "leading and trailing");
        assert_eq!(flatten(""), "");
    }

    #[test]
    fn clipping_measures_cells_and_marks_the_cut() {
        assert_eq!(clip("abcdefghij", 10), "abcdefghij");
        assert_eq!(clip("abcdefghij", 5), "abc..");
        // A two-cell glyph cannot land on an odd column edge, so the promise is "never overrun".
        assert_eq!(clip("文件文件文件", 5), "文..");
        assert!(display_width(&clip("文件文件文件", 5)) <= 5, "never overrun the column");
        assert_eq!(clip("文件", 1), "..");
    }

    #[test]
    fn character_clipping_counts_what_a_reader_would_count() {
        assert_eq!(clip_chars("abcdefghij", 6), "abcdef..");
        assert_eq!(clip_chars("文件文件文件文件文件文件文件文件", 6), "文件文件文件..");
        assert_eq!(display_width(&clip_chars("文件文件文件文件文件文件文件文件", 6)), 14, "6 glyphs plus the mark");
        assert_eq!(display_width(&clip_chars("文件", 60)), 4, "an uncut body is left whole");
        assert_eq!(clip_chars("文件", 60), "文件");
        assert_eq!(clip_chars("", 3), "");
    }

    #[test]
    fn a_table_aligns_on_display_width_not_char_count() {
        let mut t = Table::new(&["time", "title", "body"], &[Align::Left; 3]);
        t.push(vec!["10:00:00".into(), "ChatGPT".into(), "hello".into()]);
        t.push(vec!["10:05:00".into(), "QQ".into(), "文件聊天".into()]);
        let lines: Vec<String> = t.render().lines().map(str::to_string).collect();
        assert_eq!(lines[1], "--------  -------  --------");
        assert_eq!(cell_of(&lines[0], "body"), cell_of(&lines[2], "hello"));
        assert_eq!(cell_of(&lines[0], "body"), cell_of(&lines[3], "文件聊天"));
        assert_eq!(cell_of(&lines[0], "title"), cell_of(&lines[3], "QQ"));
        assert!(lines[0].ends_with("body") && lines[3].ends_with("文件聊天"), "no trailing padding: {lines:?}");
    }

    #[test]
    fn numeric_columns_are_right_aligned_and_a_header_only_table_still_draws() {
        // One column is also the last column, so it is deliberately left unpadded.
        assert_eq!(Table::new(&["n"], &[Align::Right]).render(), "n\n-");
        let mut t = Table::new(&["n", "k"], &[Align::Right, Align::Left]);
        t.push(vec!["7".into(), "x".into()]);
        t.push(vec!["1000".into(), "yy".into()]);
        assert_eq!(t.render(), "   n  k\n----  --\n   7  x\n1000  yy");
    }

    #[test]
    fn resampling_sums_rather_than_thins_out() {
        assert_eq!(resample(&[1; 240], 4), vec![60, 60, 60, 60]);
        assert_eq!(resample(&[0, 1, 2, 3], 2), vec![1, 5]);
        assert_eq!(resample(&[5, 6], 8), vec![5, 6], "fewer values than columns is not an error");
        assert_eq!(resample(&[1, 2], 0), Vec::<usize>::new());
        assert_eq!(resample(&[1; 240], 4).iter().sum::<usize>(), 240, "the total survives");
    }

    #[test]
    fn a_column_chart_is_bottom_up_and_never_hides_a_nonzero_value() {
        // Values [0, 4, 1] at height 4: the tall column reaches every row, the single hit shows on
        // the bottom row only, and the empty column stays dark.
        assert_eq!(column_chart(&[0, 4, 1], 4), vec![".#.", ".#.", ".#.", ".##"]);
        assert_eq!(column_chart(&[4, 0, 1], 4), vec!["#..", "#..", "#..", "#.#"]);
        assert_eq!(column_chart(&[2, 0], 2), vec!["#.", "#."]);
        assert_eq!(column_chart(&[], 3), vec!["", "", ""]);
        assert_eq!(column_chart(&[1], 1), vec!["#"]);
        assert_eq!(column_chart(&[0], 1), vec!["."]);
        assert_eq!(column_chart(&[9, 1], 1), vec!["##"], "height 1 still shows every hit column");
    }

    #[test]
    fn bars_are_scaled_and_never_empty_for_a_real_value() {
        assert_eq!(bar(5, 10, 10), "#####.....");
        assert_eq!(bar(1, 1000, 10), "#.........", "one hit must still be visible");
        assert_eq!(bar(0, 10, 4), "....");
        assert_eq!(bar(10, 10, 4), "####");
        assert_eq!(bar(-3, 10, 4), "....");
        assert_eq!(bar(3, 0, 4), "....");
        assert_eq!(bar(3, 10, 0), "");
    }

    #[test]
    fn a_ruler_puts_ticks_and_labels_at_the_same_positions() {
        let labels = vec!["00:00".to_string(), "12:00".to_string(), "23:59".to_string()];
        let (ticks, text) = ruler(21, &labels);
        assert_eq!(ticks, "|---------|---------|");
        assert_eq!(cell_of(&text, "00:00"), 0);
        assert_eq!(cell_of(&text, "12:00"), 8);
        assert_eq!(cell_of(&text, "23:59"), 16, "the last label is pulled inside the line");
        assert_eq!(ticks.chars().count(), text.chars().count());
        assert_eq!(ruler(4, &[] as &[String]).0, "    ");
        assert_eq!(ruler(0, &labels).0, "");
        assert_eq!(ruler(9, &["mid".to_string()]).0, "|--------");
    }

    #[test]
    fn stride_labels_spreads_the_axis_over_the_bucket_count() {
        let all: Vec<String> = (0..240).map(|i| format!("{i:03}")).collect();
        assert_eq!(stride_labels(&all, 4), vec!["000", "060", "120", "180"]);
        assert_eq!(stride_labels(&all, 300).len(), 240);
        assert_eq!(stride_labels(&[], 4), Vec::<String>::new());
        assert_eq!(stride_labels(&all, 0), Vec::<String>::new());
    }

    #[test]
    fn offsets_keep_a_negative_one_honest() {
        assert_eq!(offset(330), "+0:05:30");
        assert_eq!(offset(0), "+0:00:00");
        assert_eq!(offset(-61), "-0:01:01");
    }

    #[test]
    fn a_blank_option_is_reported_as_a_named_placeholder() {
        assert_eq!(quoted_or("ChatGPT", "<none>"), "'ChatGPT'");
        assert_eq!(quoted_or("", "<none>"), "<none>");
    }

    #[test]
    fn millis_is_fixed_width() {
        assert_eq!(millis(1.5), "   1.500 ms");
        assert_eq!(display_width(&millis(1234.5)), display_width(&millis(0.05)));
    }
}
