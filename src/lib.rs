//! # A generic fuzzy item picker
//! This is a generic picker implementation which wraps the [`nucleo::Nucleo`] matching engine. The
//! API is pretty similar to how one would use [`Nucleo`](nucleo::Nucleo).
//!
//! The majority of the internal state is re-exposed through the main [`Picker`] entrypoint.
//!
//! For usage examples, visit the [examples
//! folder](https://github.com/autobib/nucleo-picker/tree/master/examples) on GitHub.
mod bind;
pub mod component;
pub mod fill;

use std::{
    cmp::min,
    io::{self, Stdout, Write},
    process::exit,
    sync::Arc,
    thread::{available_parallelism, sleep},
    time::{Duration, Instant},
};

use crossterm::{
    cursor::{MoveTo, MoveToColumn, MoveUp},
    event::{poll, read, DisableBracketedPaste, EnableBracketedPaste},
    execute,
    style::{
        Attribute, Color, Print, PrintStyledContent, ResetColor, SetAttribute, SetForegroundColor,
        Stylize,
    },
    terminal::{
        disable_raw_mode, enable_raw_mode, size, Clear, ClearType, EnterAlternateScreen,
        LeaveAlternateScreen,
    },
    tty::IsTty,
    QueueableCommand,
};
use nucleo::{Config, Injector, Nucleo, Utf32String};

use crate::{
    bind::{convert, Event},
    component::{Edit, EditableString},
};

pub use nucleo;

/// The outcome after processing all of the events.
pub enum EventSummary {
    Continue,
    UpdateQuery(bool),
    Select,
    Quit,
}

/// A representation of the current state of the picker.
#[derive(Debug)]
struct PickerState {
    /// The width of the screen.
    width: u16,
    /// The height of the screen, including the prompt.
    height: u16,
    /// The selector index position, or [`None`] if there is nothing to select.
    selector_index: Option<u16>,
    /// The query string.
    query: EditableString,
    /// The current number of items to be drawn to the terminal.
    draw_count: u16,
    /// The total number of items.
    item_count: u32,
    /// The number of matches.
    matched_item_count: u32,
    /// Has the state changed?
    needs_redraw: bool,
}

impl PickerState {
    /// The initial picker state.
    pub fn new(dimensions: (u16, u16)) -> Self {
        let (width, height) = dimensions;
        Self {
            width,
            height,
            selector_index: None,
            query: EditableString::default(),
            draw_count: 0,
            matched_item_count: 0,
            item_count: 0,
            needs_redraw: true,
        }
    }

    /// Increment the current item selection.
    pub fn incr_selection(&mut self) {
        self.needs_redraw = true;
        self.selector_index = self.selector_index.map(|i| i.saturating_add(1));
        self.clamp_selector_index();
    }

    /// Decrement the current item selection.
    pub fn decr_selection(&mut self) {
        self.needs_redraw = true;
        self.selector_index = self.selector_index.map(|i| i.saturating_sub(1));
        self.clamp_selector_index();
    }

    /// Update the draw count from a snapshot.
    pub fn update<T: Send + Sync + 'static>(
        &mut self,
        changed: bool,
        snapshot: &nucleo::Snapshot<T>,
    ) {
        if changed {
            self.needs_redraw = true;
            self.item_count = snapshot.item_count();
            self.matched_item_count = snapshot.matched_item_count();
            self.draw_count = self.matched_item_count.try_into().unwrap_or(u16::MAX);
            self.clamp_draw_count();
            self.clamp_selector_index();
        }
    }

    /// Clamp the draw count so that it falls in the valid range.
    fn clamp_draw_count(&mut self) {
        self.draw_count = min(self.draw_count, self.height - 2)
    }

    /// Clamp the selector index so that it falls in the valid range.
    fn clamp_selector_index(&mut self) {
        if self.draw_count == 0 {
            self.selector_index = None;
        } else {
            let position = min(self.selector_index.unwrap_or(0), self.draw_count - 1);
            self.selector_index = Some(position);
        }
    }

    /// Perform the given edit action.
    pub fn edit_query(&mut self, st: Edit) {
        self.needs_redraw |= self.query.edit(st);
    }

    /// Format a [`Utf32String`] for displaying. Currently:
    /// - Delete control characters.
    /// - Truncates the string to an appropriate length.
    /// - Replaces any newline characters with spaces.
    fn format_display(&self, display: &Utf32String) -> String {
        display
            .slice(..)
            .chars()
            .filter(|ch| !ch.is_control())
            .take(self.width as usize - 2)
            .map(|ch| match ch {
                '\n' => ' ',
                s => s,
            })
            .collect()
    }

    /// Clear the queued events.
    fn handle(&mut self) -> Result<EventSummary, io::Error> {
        let mut update_query = false;
        let mut append = true;

        while poll(Duration::from_millis(5))? {
            if let Some(event) = convert(read()?) {
                match event {
                    Event::Abort => exit(1),
                    Event::MoveToStart => self.edit_query(Edit::MoveToStart),
                    Event::MoveToEnd => self.edit_query(Edit::MoveToEnd),
                    Event::Insert(ch) => {
                        update_query = true;
                        // if the cursor is at the end, it means the character was appended
                        append &= self.query.cursor_at_end();
                        self.edit_query(Edit::Insert(ch));
                    }
                    Event::Select => return Ok(EventSummary::Select),
                    Event::MoveUp => self.incr_selection(),
                    Event::MoveDown => self.decr_selection(),
                    Event::MoveLeft => self.edit_query(Edit::MoveLeft),
                    Event::MoveRight => self.edit_query(Edit::MoveRight),
                    Event::Delete => {
                        update_query = true;
                        append = false;
                        self.edit_query(Edit::Delete);
                    }
                    Event::Quit => return Ok(EventSummary::Quit),
                    Event::Resize(width, height) => {
                        self.resize(width, height);
                    }
                    Event::Paste(contents) => {
                        update_query = true;
                        append &= self.query.cursor_at_end();
                        self.edit_query(Edit::Paste(contents));
                    }
                }
            }
        }
        Ok(if update_query {
            EventSummary::UpdateQuery(append)
        } else {
            EventSummary::Continue
        })
    }

    /// Draw the terminal to the screen. This assumes that the draw count has been updated and the
    /// selector index has been properly clamped, or this method will panic!
    pub fn draw<T: Send + Sync + 'static>(
        &mut self,
        stdout: &mut Stdout,
        snapshot: &nucleo::Snapshot<T>,
    ) -> Result<(), io::Error> {
        if self.needs_redraw {
            // reset redraw state
            self.needs_redraw = false;

            // clear screen and set cursor position to bottom
            stdout
                .queue(Clear(ClearType::All))?
                .queue(MoveTo(0, self.height - 2))?;

            // draw the match counts
            stdout
                .queue(SetAttribute(Attribute::Italic))?
                .queue(SetForegroundColor(Color::Green))?
                .queue(Print("  "))?
                .queue(Print(self.matched_item_count))?
                .queue(Print("/"))?
                .queue(Print(self.item_count))?
                .queue(SetAttribute(Attribute::Reset))?
                .queue(ResetColor)?;

            // draw the matches
            for (idx, it) in snapshot.matched_items(..self.draw_count as u32).enumerate() {
                let render = self.format_display(&it.matcher_columns[0]);
                if Some(idx) == self.selector_index.map(|i| i as _) {
                    stdout
                        .queue(SetAttribute(Attribute::Bold))?
                        .queue(MoveUp(1))?
                        .queue(MoveToColumn(2))?
                        .queue(Print(render))?
                        .queue(SetAttribute(Attribute::Reset))?;
                } else {
                    stdout
                        .queue(MoveUp(1))?
                        .queue(MoveToColumn(2))?
                        .queue(Print(render))?;
                }
            }

            // draw the selection indicator
            if let Some(position) = self.selector_index {
                stdout
                    .queue(MoveTo(0, self.height - 3 - position))?
                    .queue(PrintStyledContent("▌".with(Color::Magenta)))?;
            }

            // render the query string
            stdout
                .queue(MoveTo(0, self.height - 1))?
                .queue(Print("> "))?
                .queue(Print(&self.query))?
                .queue(MoveTo(self.query.position() as u16 + 2, self.height - 1))?;

            // flush to terminal
            stdout.flush()
        } else {
            Ok(())
        }
    }

    /// Resize the terminal state on screen size change.
    pub fn resize(&mut self, width: u16, height: u16) {
        self.needs_redraw = true;
        self.width = width;
        self.height = height;
        self.clamp_draw_count();
        self.clamp_selector_index();
    }
}

/// # Core picker struct
pub struct Picker<T: Send + Sync + 'static> {
    matcher: Nucleo<T>,
}

impl<T: Send + Sync + 'static> Default for Picker<T> {
    fn default() -> Self {
        Self::new(Config::DEFAULT, Self::suggested_threads(), 1)
    }
}

impl<T: Send + Sync + 'static> Picker<T> {
    /// Best-effort guess to reduce thread contention. Reserve two threads:
    /// 1. for populating the macher
    /// 2. for rendering the terminal UI and handling user input
    fn suggested_threads() -> Option<usize> {
        available_parallelism()
            .map(|it| it.get().checked_sub(2).unwrap_or(1))
            .ok()
    }

    /// Suggested frame length of 16ms, or ~60 FPS.
    const fn suggested_frame_interval() -> Duration {
        Duration::from_millis(16)
    }

    /// Create a new [`Picker`] instance with arguments passed to [`Nucleo`](Nucleo).
    pub fn new(config: Config, num_threads: Option<usize>, columns: u32) -> Self {
        Self {
            matcher: Nucleo::new(config, Arc::new(|| {}), num_threads, columns),
        }
    }

    /// Create a new [`Picker`] instance with the given configuration.
    pub fn with_config(config: Config) -> Self {
        Self {
            matcher: Nucleo::new(config, Arc::new(|| {}), Self::suggested_threads(), 1),
        }
    }

    /// Get an [`Injector`] from the internal [`Nucleo`] instance.
    pub fn injector(&self) -> Injector<T> {
        self.matcher.injector()
    }

    /// Open the picker prompt for user interaction and return the picked item, if any.
    pub fn pick(&mut self) -> Result<Option<&T>, io::Error> {
        if !std::io::stdin().is_tty() {
            return Err(io::Error::new(io::ErrorKind::Other, "is not interactive"));
        }

        self.pick_inner(Self::suggested_frame_interval())
    }

    /// The actual picker implementation.
    fn pick_inner(
        &mut self,
        // events: Receiver<Event>,
        interval: Duration,
    ) -> Result<Option<&T>, io::Error> {
        let mut stdout = io::stdout();
        let mut term = PickerState::new(size()?);

        enable_raw_mode()?;
        execute!(stdout, EnterAlternateScreen, EnableBracketedPaste)?;

        let selection = loop {
            let deadline = Instant::now() + interval;

            // process any queued keyboard events and reset query pattern if necessary
            match term.handle()? {
                EventSummary::Continue => {}
                EventSummary::UpdateQuery(append) => {
                    self.matcher.pattern.reparse(
                        0,
                        &term.query.to_string(),
                        nucleo::pattern::CaseMatching::Smart,
                        nucleo::pattern::Normalization::Smart,
                        append,
                    );
                }
                EventSummary::Select => {
                    break term
                        .selector_index
                        .and_then(|idx| self.matcher.snapshot().get_matched_item(idx as _))
                        .map(|it| it.data);
                }
                EventSummary::Quit => {
                    break None;
                }
            };

            // increment the matcher and update state
            let status = self.matcher.tick(10);
            term.update(status.changed, self.matcher.snapshot());

            // redraw the screen
            term.draw(&mut stdout, self.matcher.snapshot())?;

            // wait if frame rendering finishes early
            sleep(deadline - Instant::now());
        };

        disable_raw_mode()?;
        execute!(stdout, DisableBracketedPaste, LeaveAlternateScreen)?;
        Ok(selection)
    }
}
