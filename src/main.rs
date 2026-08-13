use std::{
    collections::VecDeque,
    env,
    fs::File,
    io::{self, Write, stdout},
    path::Path,
};

use crossterm::{
    cursor::{MoveToColumn, MoveToRow},
    event::{Event, KeyCode, KeyEventKind, KeyModifiers, read},
    execute, queue,
    style::Print,
    terminal::{
        self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode,
        enable_raw_mode,
    },
};
use memmap::{Mmap, MmapOptions};
use unicode_width::UnicodeWidthChar;

struct PagerInstance {
    rows: u16,
    cols: u16,
    mmap: Mmap,
    line_indices: Vec<usize>,
    display_window_start: usize,
    render_buffer: VecDeque<RenderLine>,
    _terminal: TerminalSession,
}

#[derive(Debug, PartialEq, Eq)]
struct RenderLine {
    start: usize,
    end: usize,
}

#[derive(Debug)]
enum PagerError {
    Io(io::Error),
    InvalidUsage,
    InvalidUtf8,
}

impl From<io::Error> for PagerError {
    fn from(value: io::Error) -> Self {
        PagerError::Io(value)
    }
}

/// Owns all process-wide terminal state changed by the pager.
struct TerminalSession;

impl TerminalSession {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;

        if let Err(error) = execute!(stdout(), EnterAlternateScreen) {
            let _ = disable_raw_mode();
            return Err(error);
        }

        Ok(Self)
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        // Drop cannot report errors, so cleanup is best-effort. Attempt both
        // operations even if leaving the alternate screen fails.
        let _ = execute!(stdout(), LeaveAlternateScreen);
        let _ = disable_raw_mode();
    }
}

impl PagerInstance {
    fn new() -> Result<Self, PagerError> {
        let file_path = env::args().nth(1).ok_or(PagerError::InvalidUsage)?;
        let path = Path::new(&file_path);
        if !path.is_file() {
            return Err(PagerError::InvalidUsage);
        }

        let file = File::open(path)?;
        let mmap = unsafe { MmapOptions::new().map(&file)? };
        let line_indices = line_indices(&mmap);
        let (cols, rows) = terminal::size()?;

        // Make terminal mutation the final initialization step. This local
        // guard restores the terminal if clearing or construction fails.
        let terminal = TerminalSession::enter()?;
        execute!(
            stdout(),
            Clear(ClearType::All),
            Clear(ClearType::Purge),
            MoveToColumn(0),
            MoveToRow(0),
        )?;

        Ok(Self {
            rows,
            cols,
            mmap,
            line_indices,
            display_window_start: 0,
            render_buffer: VecDeque::new(),
            _terminal: terminal,
        })
    }

    fn render_lines(&self) -> Result<(), PagerError> {
        let mut stdout = stdout();
        let end_index = self
            .display_window_start
            .saturating_add(self.rows as usize)
            .min(self.render_buffer.len());

        for line in self
            .render_buffer
            .range(self.display_window_start..end_index)
        {
            let decoded_str = std::str::from_utf8(&self.mmap[line.start..line.end])
                .map_err(|_| PagerError::InvalidUtf8)?;
            queue!(stdout, Print(decoded_str), Print("\n"))?;
        }

        stdout.flush()?;
        Ok(())
    }

    fn generate_utf8_line(&mut self, index: usize) -> Result<(), PagerError> {
        let start = self.line_indices[index];
        let end = self
            .line_indices
            .get(index + 1)
            .copied()
            .unwrap_or(self.mmap.len());
        append_wrapped_lines(&self.mmap, start, end, self.cols, &mut self.render_buffer)
    }

    fn clamp_display_window(&mut self) {
        self.display_window_start = clamped_window_start(
            self.display_window_start,
            self.render_buffer.len(),
            self.rows,
        );
    }

    fn render(&mut self) -> Result<(), PagerError> {
        execute!(
            stdout(),
            Clear(ClearType::All),
            MoveToRow(0),
            MoveToColumn(0),
        )?;

        self.render_buffer.clear();
        for index in 0..self.line_indices.len() {
            self.generate_utf8_line(index)?;
        }
        self.clamp_display_window();
        self.render_lines()
    }

    fn run(&mut self) -> Result<(), PagerError> {
        self.render()?;

        loop {
            match read()? {
                Event::Resize(cols, rows) => {
                    self.rows = rows;
                    self.cols = cols;
                    self.render()?;
                }
                Event::Key(key)
                    if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
                {
                    match key.code {
                        KeyCode::Char('q') => break,
                        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            break;
                        }
                        KeyCode::Char('j')
                            if self.display_window_start.saturating_add(self.rows as usize)
                                < self.render_buffer.len() =>
                        {
                            self.display_window_start += 1;
                            self.render()?;
                        }
                        KeyCode::Char('k') if self.display_window_start > 0 => {
                            self.display_window_start -= 1;
                            self.render()?;
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }

        Ok(())
    }
}

fn line_indices(data: &[u8]) -> Vec<usize> {
    let mut indices = vec![0];
    indices.extend(
        data.iter()
            .enumerate()
            .filter_map(|(index, byte)| (*byte == b'\n').then_some(index + 1)),
    );
    indices
}

fn clamped_window_start(current: usize, line_count: usize, rows: u16) -> usize {
    current.min(line_count.saturating_sub(rows as usize))
}
fn append_wrapped_lines(
    data: &[u8],
    start: usize,
    end: usize,
    cols: u16,
    output: &mut VecDeque<RenderLine>,
) -> Result<(), PagerError> {
    let text = std::str::from_utf8(&data[start..end]).map_err(|_| PagerError::InvalidUtf8)?;
    let mut current_col = 0;
    let mut segment_start = start;

    for (offset, ch) in text.char_indices() {
        let global_index = start + offset;
        if ch == '\n' {
            output.push_back(RenderLine {
                start: segment_start,
                end: global_index,
            });
            segment_start = global_index + ch.len_utf8();
            continue;
        }

        let width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if current_col + width > cols as usize && global_index > segment_start {
            output.push_back(RenderLine {
                start: segment_start,
                end: global_index,
            });
            current_col = 0;
            segment_start = global_index;
        }
        current_col += width;
    }

    if segment_start < end {
        output.push_back(RenderLine {
            start: segment_start,
            end,
        });
    }

    Ok(())
}

fn main() {
    match PagerInstance::new() {
        Ok(mut pager) => {
            if let Err(error) = pager.run() {
                drop(pager);
                eprintln!("Pager error: {error:?}");
            }
        }
        Err(PagerError::InvalidUsage) => eprintln!("Usage: winpager <filename>"),
        Err(PagerError::Io(error)) => eprintln!("Failed to start pager: {error}"),
        Err(error) => eprintln!("Failed to start pager: {error:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_each_physical_line() {
        assert_eq!(line_indices(b"first\nsecond\n"), vec![0, 6, 13]);
        assert_eq!(line_indices(b""), vec![0]);
    }

    #[test]
    fn wraps_at_character_boundaries_using_display_width() {
        let data = "ab界c\n".as_bytes();
        let mut lines = VecDeque::new();

        append_wrapped_lines(data, 0, data.len(), 3, &mut lines).unwrap();

        assert_eq!(
            lines,
            VecDeque::from([
                RenderLine { start: 0, end: 2 },
                RenderLine { start: 2, end: 6 },
            ])
        );
        assert_eq!(&data[lines[0].start..lines[0].end], b"ab");
        assert_eq!(
            std::str::from_utf8(&data[lines[1].start..lines[1].end]).unwrap(),
            "界c"
        );
    }

    #[test]
    fn preserves_blank_lines() {
        let data = b"\n";
        let mut lines = VecDeque::new();

        append_wrapped_lines(data, 0, data.len(), 80, &mut lines).unwrap();

        assert_eq!(lines, VecDeque::from([RenderLine { start: 0, end: 0 }]));
    }

    #[test]
    fn invalid_utf8_is_reported() {
        let mut lines = VecDeque::new();
        let error = append_wrapped_lines(&[0xff], 0, 1, 80, &mut lines).unwrap_err();

        assert!(matches!(error, PagerError::InvalidUtf8));
    }

    #[test]
    fn scroll_position_clamps_after_reflow() {
        assert_eq!(clamped_window_start(20, 12, 10), 2);
        assert_eq!(clamped_window_start(5, 20, 10), 5);
        assert_eq!(clamped_window_start(5, 3, 10), 0);
    }
}
