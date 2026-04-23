use std::{collections::VecDeque, env, fs::File, io::{self, Write, stdout}, path};
use crossterm::{
    cursor::{MoveToColumn, MoveToRow}, event::{Event, KeyCode, KeyModifiers, read}, execute, queue, style::Print, terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode}
};
use log::{error};
use memmap::{Mmap, MmapOptions};
use unicode_width::UnicodeWidthChar;

struct PagerInstance {
    rows: u16,
    cols: u16,
    mmap: Mmap,
    line_indices: Vec<usize>,
    display_window_start: usize,
    render_buffer: VecDeque<RenderLine>,
}

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

impl PagerInstance {
    fn new() -> Result<PagerInstance, PagerError> {
        if let Some(file_path) = env::args().nth(1) {
            // here we try to enable raw mode
            enable_raw_mode()?;
            let mut stdout = io::stdout();

            // change to alternate screen
            execute!(
                stdout,
                EnterAlternateScreen,
            )?;

            // clear the screen
            execute!(
                stdout,
                Clear(ClearType::All),
                Clear(ClearType::Purge),
                MoveToColumn(0),
                MoveToRow(0),
            )?;

            // we will try to open the file and keep the file handle in the PagerInstance struct
            let path = path::Path::new(&file_path);
            if !path.is_file() {
                return Err(PagerError::InvalidUsage);
            }

            // we will generate memory mapping for the file
            let file = File::open(&path)?;
            let mmap = unsafe {MmapOptions::new().map(&file)?};

            let mut line_indices: Vec<usize> = vec![0];
            for (i, &byte) in mmap.iter().enumerate() {
                if byte == b'\n' {
                    line_indices.push(i + 1);
                }
            }

            // we will note the positions of all the line endings   

            let (cols, rows) = terminal::size()?;
            Ok(PagerInstance {
                rows, 
                cols,
                mmap,
                line_indices,
                display_window_start: 0,
                render_buffer: VecDeque::new(),
            })
        } else {
            Err(PagerError::InvalidUsage)
        }
    }

    fn render_lines(&self) -> Result<(), PagerError> {
        let mut stdout = std::io::stdout();
        let start_index = self.display_window_start;
        let end_index = (start_index + self.rows as usize).min(self.render_buffer.len());
        for line in self.render_buffer.range(start_index..end_index) {
            let decoded_str = std::str::from_utf8(&self.mmap[line.start..line.end]).map_err(|_e| PagerError::InvalidUtf8)?;
            //println!("{}", decoded_str);
            queue!(
                stdout,
                Print(decoded_str),
                Print("\n"),
            )?;
        }

        stdout.flush()?;
        Ok(())
    }

    fn generate_utf8_line(&mut self, i: usize) -> Result<(), PagerError> {
        // we take a slice from memory map, which will be a line
        // we divide the line into multiple lines counting the no of cells requried by each char
        // if it exceeds the col count, we insert the data to the double ended queue
        // continue until the line is consumed
        
        let start = self.line_indices[i];
        let end = if i + 1 < self.line_indices.len() {
            self.line_indices[i + 1]
        } else {
            self.mmap.len()
        };

        let data = &self.mmap[start..end];

        let utf8_repr = std::str::from_utf8(data).map_err(|_e| PagerError::InvalidUtf8)?;
        let mut curr_col = 0;
        let mut seg_start = start;
        for (offset, ch) in utf8_repr.char_indices() {
            let global_idx = start + offset;
            if ch == '\n' {
                self.render_buffer.push_back(RenderLine { start: seg_start, end:  global_idx});
                curr_col = 0;
                seg_start = global_idx + ch.len_utf8();
                continue;
            }
            let width = UnicodeWidthChar::width(ch).unwrap_or(0);
            if curr_col + width > self.cols as usize {
                self.render_buffer.push_back(RenderLine { start: seg_start, end:  global_idx});
                curr_col = 0;
                seg_start = global_idx;
            }

            curr_col += width;
        }

        if seg_start <  start + data.len() {
            self.render_buffer.push_back(RenderLine {
                start: seg_start,
                end: start + data.len(),
            });
        }
        Ok(())

    }

    fn render(&mut self) -> Result<(), PagerError>{
        execute!(
            stdout(),
            Clear(ClearType::All),
            MoveToRow(0),
            MoveToColumn(0),
        )?;
        self.render_buffer.clear();
        for i in 0..self.line_indices.len() {
            self.generate_utf8_line(i)?;
        }
        self.render_lines()?;
        Ok(())
    }

    fn run(&mut self) -> Result<(), PagerError> {
        // we know the number of rows
        // if we have less lines than number of rows, we dont have to worry about scrolling
        self.render()?;
        loop {
            match read()? {
                Event::Resize(cols, rows) => {
                    self.rows = rows;
                    self.cols = cols;
                    self.render()?;
                },
                Event::Key(code) => {
                    match code.code {
                        KeyCode::Char(chr) => {
                            if chr == 'q' {
                                break;
                            } else if chr == 'c' {
                                // check if modifier was there
                                if code.modifiers == KeyModifiers::CONTROL {
                                    continue;
                                }
                            } else if chr == 'j' {
                                execute!(
                                    stdout(),
                                    MoveToRow(10),
                                    MoveToColumn(20),
                                    Print(self.render_buffer.len())
                                )?;
                                if (self.display_window_start + self.rows as usize) < self.render_buffer.len() {
                                    self.display_window_start += 1;
                                    self.render()?;
                                }
                            } else if chr == 'k' {
                                if self.display_window_start > 0 {
                                    self.display_window_start -= 1;
                                    self.render()?;
                                }
                            }
                        },
                        _ => {},
                    }
                },
                _ => {},
            }
        }

        Ok(())
    }
}

impl Drop for PagerInstance {
    fn drop(&mut self) {
        if let Err(err) = execute!(io::stdout(), LeaveAlternateScreen) {
            error!("Failed to restore the original screen: {}\n", err)
        }

        if let Err(err) = disable_raw_mode() {
            error!("Failed to disable raw mode: {}\n", err);
        }
            
    }
}

fn main() {
    if let Ok(mut pager) = PagerInstance::new() {
        match pager.run() {
            Ok(_) => {},
            Err(err) => match err {
                PagerError::Io(err) => println!("{:?}", err),
                _ => println!("{:?}", err),
            },
        }
    } else {
        println!("Usage: winpager <filename>")
    }
}