//! The window's layout.
//!
//! Screens are built from a small set of shared pieces — the top bar, cards
//! and a table of files — so the flow reads as one application rather than
//! five separate forms.

use std::path::PathBuf;
use std::time::Duration;

use iced::widget::{
    button, checkbox, column, container, pick_list, row, scrollable, text, text_input,
};
use iced::{Alignment, Color, Element, Fill, Length, Theme};

use packrat_core::{DiscKind, OpticalDrive, Title};

use crate::jobs::{self, Job, JobStatus};
use crate::theme::{self, ACCENT, DANGER, MUTED, SUCCESS, TEXT, WARNING};
use crate::{App, Message, MetaState, Stage};

/// Caption text: labels, table headers, secondary detail.
const CAPTION: f32 = 11.0;
/// Body text.
const BODY: f32 = 13.0;
/// Card titles.
const HEADING: f32 = 17.0;
/// The state heading in the top bar.
const TITLE: f32 = 20.0;

/// The whole window.
pub fn view(app: &App) -> Element<'_, Message> {
    // Settings is the one thing that is not the backup screen: it is where the
    // destinations are chosen, and it is reachable from anywhere.
    let content: Element<'_, Message> = if app.stage == Stage::Setup {
        settings(app)
    } else {
        backup(app)
    };

    // One bar at the top and nothing at the bottom, so a long page runs to the
    // window edge and the state stays put whatever the page is scrolled to.
    let body = column![
        top_bar(app),
        divider(),
        scrollable(container(content).padding(22).width(Fill))
            .style(theme::scrollbar)
            .height(Fill),
    ];

    container(body)
        .style(theme::page)
        .width(Fill)
        .height(Fill)
        .into()
}

/// The heading over the page: which of the states the window is in.
///
/// One word or two, because it is read at a glance and the line under it says
/// the rest.
fn top_heading(app: &App) -> (&'static str, Color) {
    match app.stage {
        Stage::Setup => ("Settings", TEXT),
        Stage::Ripping => ("Backing up", ACCENT),
        Stage::Done if app.cancelled => ("Stopped", WARNING),
        Stage::Done if app.failed > 0 => ("Finished with errors", DANGER),
        Stage::Done => ("Backup complete", SUCCESS),
        _ if app.disc.is_some() => ("Ready to back up", TEXT),
        _ => ("No disc", MUTED),
    }
}

/// The line under the heading: what is happening, or how the last run went.
pub(crate) fn top_detail(app: &App) -> String {
    if app.stage == Stage::Done {
        return done_detail(app);
    }
    if app.stage != Stage::Ripping {
        return stage_hint(app);
    }

    // `done` counts every finished file, failures included.
    let mut line = format!("{} of {}", (app.done + 1).min(app.total), app.total);
    if let Some(progress) = &app.progress {
        // Which half is working: the drive is the slow one, so it is worth
        // knowing when the wait is the disc and when it is us.
        line.push_str(match progress.phase {
            packrat_core::RemuxPhase::Probing => " · reading the disc",
            packrat_core::RemuxPhase::Muxing => " · writing the file",
        });
        if let Some(rate) = progress.read_rate() {
            line.push_str(&format!(" · {} /s", jobs::fmt_bytes(rate as u64)));
        }
        if let Some(left) = progress.eta() {
            line.push_str(&format!(
                " · {} left in this file",
                jobs::fmt_duration(Some(left))
            ));
        }
    }
    // Said last and said plainly, because a drive that has stopped answering is
    // the reason none of the figures above are moving.
    if let Some(quiet) = app.quiet_for() {
        line.push_str(&format!(" · {}", stall_note(quiet)));
    }

    line
}

/// What the window says when the drive has stopped answering.
///
/// The app cannot interrupt a read the kernel is still waiting on, so the honest
/// thing is to say which of the two is happening: the drive is being waited on,
/// and stopping will not get out of it any faster.
fn stall_note(quiet: Duration) -> String {
    format!(
        "the drive has not answered for {}s; a read in the kernel cannot be interrupted",
        quiet.as_secs()
    )
}

/// What the last run came to: the run's own figures, under the verdict.
///
/// This is the whole report of a finished run, which is why there is no result
/// card on the page: the state of the window belongs in the bar that states it.
fn done_detail(app: &App) -> String {
    let mut line = format!(
        "{} of {} files · {} written",
        app.done.saturating_sub(app.failed),
        app.total,
        jobs::fmt_bytes(app.session_bytes)
    );
    if app.failed > 0 {
        line.push_str(&format!(" · {} failed", app.failed));
    }
    if let Some(elapsed) = app.rip_elapsed {
        line.push_str(&format!(" · took {}", jobs::fmt_duration(Some(elapsed))));
    }
    if app.ejected {
        line.push_str(" · disc ejected");
    }
    line
}

/// The one backup screen, ordered as the work is done: choose a disc, see what
/// is on it, say how it should be named, then back it up.
///
/// Everything is on this page rather than behind steps, so the effect of a
/// change — the verdict, the metadata, the selections — is visible without
/// navigating to find it, and nothing above the files moves while a backup runs.
fn backup(app: &App) -> Element<'_, Message> {
    let mut body = column![].spacing(16);

    // Only worth saying while there is no disc card to carry it.
    if app.disc.is_none() {
        if let Some(note) = &app.css_note {
            body = body.push(notice_block(note));
        }
    }

    // 1. Which disc to read: one field, and the picker for it.
    body = body.push(choose_disc_card(app));

    if app.disc.is_some() {
        // 2. What it is and what to call it: the identity the disc was read as,
        //    over the fields that correct it.
        body = body.push(disc_card(app));
        // 3. Every title on the disc and what will be made of it: the tracks
        //    and the selection are the same rows.
        body = body.push(titles_card(app));
    }

    body.into()
}

/// What the action bar says: the last thing that happened, or what to do next.
fn stage_hint(app: &App) -> String {
    // A drive that has stopped answering outranks every other note: it is the
    // thing that is true right now, and it explains why nothing else is moving.
    if let Some(quiet) = app.quiet_for() {
        return stall_note(quiet);
    }
    // The watcher's note next: it is the only one about right now, and it is
    // taken back the moment it stops applying, so it cannot hide anything for
    // longer than it is true.
    if let Some(note) = app.watch_note.clone() {
        return note;
    }
    if let Some(note) = app
        .eject_note
        .clone()
        .or_else(|| app.meta_note.clone())
        .or_else(|| app.detect_note.clone())
        .or_else(|| app.config_note.clone())
    {
        return note;
    }

    match app.stage {
        Stage::Setup => "Choose where backups go. You can change this at any time.".to_string(),
        Stage::Ripping => {
            "Backing up. Stopping leaves the file being written incomplete.".to_string()
        }
        // The bar's own stats line says what became of it.
        Stage::Done => String::new(),
        _ if app.detect_loading => "Reading the disc…".to_string(),
        _ if app.disc.is_none() => {
            "Choose a drive or a folder. A disc put into the chosen drive is read on its own."
                .to_string()
        }
        _ => "Untick anything you do not want, then back up.".to_string(),
    }
}

// ---------------------------------------------------------------------------
// Shared pieces
// ---------------------------------------------------------------------------

/// The top bar: what the app is doing and how far along, and settings.
///
/// The state lives up here because it is true of the whole window rather than of
/// any one section of it, and it costs no vertical space that a card would.
/// The window's chrome: the state, what is happening, and every button that is
/// not about one section of the page.
///
/// The main call to action is up here too, so the thing to do next is in the
/// same place whatever the page is scrolled to, and a run is stopped from the
/// same bar that reports it.
fn top_bar(app: &App) -> Element<'_, Message> {
    let detail = top_detail(app);
    let line = text(detail).size(CAPTION).color(MUTED).width(Fill);

    let (heading, colour) = top_heading(app);
    let state = column![text(heading).size(TITLE).color(colour), line]
        .spacing(4)
        .width(Fill);

    let mut actions = row![].spacing(10).align_y(Alignment::Center);
    match app.stage {
        Stage::Ripping => {
            actions = actions.push(
                button(text("Stop now").size(BODY))
                    .on_press(Message::AbortRip)
                    .style(theme::danger)
                    .padding(9),
            );
        }
        // The end of a run: one way on. The plan it came from is still there,
        // with the disc if it is still in the drive and the picker if it is not,
        // so leaving the result is the same act either way. Eject stays with
        // the disc it would act on.
        Stage::Done => {
            actions = actions.push(
                button(text("Done").size(BODY))
                    .on_press(Message::BackToPlan)
                    .style(theme::primary)
                    .padding(9),
            );
        }
        Stage::Setup => {
            let first_run = app.config_return == Stage::Detect && app.disc.is_none();
            actions = actions
                .push(
                    button(text(if first_run { "Skip for now" } else { "Back" }).size(BODY))
                        .on_press(Message::CloseSettings)
                        .style(theme::ghost)
                        .padding(9),
                )
                .push(
                    button(
                        text(if first_run {
                            "Save and continue"
                        } else {
                            "Save"
                        })
                        .size(BODY),
                    )
                    .on_press(Message::SaveAndClose)
                    .style(theme::primary)
                    .padding(9),
                );
        }
        // The thing to do next, where the state of the run is stated. Greyed
        // out rather than absent when there is nothing to back up.
        _ => {
            if app.disc.is_some() {
                actions = actions.push(
                    button(text("Back up now").size(BODY))
                        .on_press_maybe(app.has_selection().then_some(Message::StartRip))
                        .style(theme::primary)
                        .padding(9),
                );
            }
        }
    }
    if app.stage != Stage::Setup {
        actions = actions.push(
            button(text("Settings").size(BODY))
                .on_press(Message::OpenSettings)
                .style(theme::ghost)
                .padding(9),
        );
    }

    container(row![state, actions].spacing(20).align_y(Alignment::Center))
        .style(theme::bar)
        .padding([14, 22])
        .width(Fill)
        .into()
}

/// A raised panel with an optional title.
fn card<'a>(title: &'a str, content: Element<'a, Message>) -> Element<'a, Message> {
    let mut inner = column![].spacing(12);
    if !title.is_empty() {
        inner = inner.push(text(title).size(HEADING));
    }
    inner = inner.push(content);

    container(inner)
        .style(theme::card)
        .padding(18)
        .width(Fill)
        .into()
}

/// A small pill, used to state a disc's kind or a drive's state.
fn badge(label: String, color: Color) -> Element<'static, Message> {
    container(text(label).size(CAPTION).color(color))
        .style(theme::badge(color))
        .padding(6)
        .into()
}

/// An inset notice for a condition the user should know about.
fn notice_block<'a>(message: &'a str) -> Element<'a, Message> {
    container(
        row![
            text("!").size(BODY).color(WARNING),
            text(message).size(CAPTION).color(WARNING).width(Fill),
        ]
        .spacing(10)
        .align_y(Alignment::Center),
    )
    .style(theme::notice(WARNING))
    .padding(11)
    .width(Fill)
    .into()
}

/// Placeholder content for an empty list.
fn empty<'a>(message: &'a str) -> Element<'a, Message> {
    container(text(message).size(BODY).color(MUTED))
        .style(theme::row_alt)
        .padding(16)
        .width(Fill)
        .into()
}

/// A hairline divider.
fn divider() -> Element<'static, Message> {
    container(text(""))
        .style(theme::row_alt)
        .height(Length::Fixed(1.0))
        .width(Fill)
        .into()
}

/// A label/value pair, with the value left-aligned beside a fixed label column
/// so a stack of them scans as a list rather than a gap.
fn kv<'a>(label: &'a str, value: String) -> Element<'a, Message> {
    row![
        text(label)
            .size(CAPTION)
            .color(MUTED)
            .width(Length::Fixed(72.0)),
        text(value).size(BODY),
    ]
    .spacing(10)
    .into()
}

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

fn settings(app: &App) -> Element<'_, Message> {
    let mut form = column![
        text("Where backups go").size(HEADING),
        text(
            "Point packrat at the folders that already hold your show and movie \
             folders — an existing Plex library works as-is."
        )
        .size(CAPTION)
        .color(MUTED),
        labelled_input(
            "TV directory",
            "/mnt/media/tv",
            &app.tv_input,
            Message::TvDirChanged,
            Fill,
            true,
        ),
        labelled_input(
            "Movie directory",
            "/mnt/media/movies",
            &app.movie_input,
            Message::MovieDirChanged,
            Fill,
            true,
        ),
        labelled_input(
            "TMDb API key (optional, so movies are named from TMDb)",
            "paste a key to match movie titles",
            &app.tmdb_input,
            Message::TmdbKeyChanged,
            Fill,
            true,
        ),
        checkbox(app.auto_eject)
            .label("Eject when done")
            .style(theme::check)
            .on_toggle(Message::AutoEjectToggled),
    ]
    .spacing(14);

    if let Some(note) = &app.config_note {
        form = form.push(text(note.clone()).size(CAPTION).color(MUTED));
    }

    // Settings is the one place with room for the collection figures, which
    // would otherwise sit in front of every screen.
    container(
        row![
            container(form)
                .style(theme::card)
                .padding(24)
                .width(Length::Fixed(580.0)),
            container(collection_card(app)).width(Length::Fixed(340.0)),
        ]
        .spacing(16),
    )
    .center_x(Fill)
    .into()
}

/// What is already on disk, and what packrat has written.
fn collection_card(app: &App) -> Element<'_, Message> {
    let stats = &app.library;
    let mut body = column![].spacing(10);

    if stats.is_empty() {
        body = body.push(
            text("Nothing indexed yet. Once a disc is backed up, the library shows up here.")
                .size(CAPTION)
                .color(MUTED),
        );
    } else {
        body = body.push(section_label("On disk"));
        body = body.push(kv("Shows", stats.shows.to_string()));
        body = body.push(kv("Seasons", stats.seasons.to_string()));
        body = body.push(kv("Episodes", stats.episodes.to_string()));
        body = body.push(kv("Movies", stats.movies.to_string()));
        body = body.push(kv("Size", jobs::fmt_bytes(stats.total_bytes())));
    }

    if let Some(free) = app.free_bytes {
        body = body.push(kv("Free space", jobs::fmt_bytes(free)));
    }

    body = body.push(divider());
    body = body.push(section_label("Backed up by packrat"));
    if app.history.discs == 0 {
        body = body.push(text("Nothing yet.").size(CAPTION).color(MUTED));
    } else {
        body = body.push(kv("Discs", app.history.discs.to_string()));
        body = body.push(kv("Files", app.history.files.to_string()));
        body = body.push(kv("Written", jobs::fmt_bytes(app.history.bytes)));
    }

    card("Collection", body.into())
}

fn labelled_input<'a>(
    label: &'a str,
    placeholder: &'a str,
    value: &'a str,
    on_input: fn(String) -> Message,
    width: Length,
    enabled: bool,
) -> Element<'a, Message> {
    column![
        text(label).size(CAPTION).color(MUTED),
        text_input(placeholder, value)
            .on_input_maybe(enabled.then_some(on_input))
            .on_submit_maybe(enabled.then_some(Message::RefreshMetadata))
            .style(theme::input)
            .padding(10)
            .width(width),
    ]
    .spacing(5)
    .into()
}

// ---------------------------------------------------------------------------
// Drive detection
// ---------------------------------------------------------------------------

/// The disc: where it comes from on the left, what it turned out to be on the
/// right.
///
/// This is the first thing on the page because it is the first thing the user
/// does, and the identity appears inside the same card rather than on a screen
/// of its own once there is one to show.
/// Which disc to read, and the first thing on the page.
///
/// One field holding a path — a device or a folder with a `VIDEO_TS` in it —
/// with the picker beside it. A drive the scan finds fills the field in, so
/// there is one thing to read whether the disc was found or typed, and no
/// second input to reconcile it with.
fn choose_disc_card(app: &App) -> Element<'_, Message> {
    // While a run is going the disc cannot be changed out from under it, so the
    // field stays exactly where it is and goes grey. A pick list cannot be
    // disabled — it always opens — so it is drawn as the field it would have
    // been, and the handler refuses the message either way.
    let busy = app.ripping();

    let picker: Element<'_, Message> = if busy {
        disabled_field(
            selected_choice(app)
                .map(|choice| choice.to_string())
                .unwrap_or_else(|| "No disc".to_string()),
        )
    } else {
        // One list of everything a disc can be read from: the drives the scan
        // found, then whatever the user has opened through the picker. Choosing
        // an entry loads it, so there is nothing to reconcile or press after.
        pick_list(
            disc_choices(app),
            selected_choice(app),
            |choice: DiscChoice| match choice {
                DiscChoice::Drive(drive) => Message::DriveSelected(drive.index),
                DiscChoice::Folder(path) => Message::DiscPathSelected(path),
            },
        )
        .placeholder("Choose a disc…")
        .style(theme::dropdown)
        .text_size(BODY)
        .padding(10)
        .width(Fill)
        .into()
    };

    let field = row![
        picker,
        button(text("Browse").size(BODY))
            .on_press_maybe((!busy).then_some(Message::Browse))
            .style(theme::ghost)
            .padding(10),
        // Both sit beside the picker rather than under it: they are the things
        // you do to the disc, and the line below describes it. There is no
        // rescan button because the window watches its drive on a timer and
        // retries by itself.
        //
        // Eject is always here, so it does not appear and disappear under the
        // pointer: greyed out when there is no disc to eject, or while a run is
        // reading the disc it would eject.
        button(text("Eject").size(BODY))
            .on_press_maybe(if busy {
                None
            } else {
                ejectable_device(app).map(Message::Eject)
            })
            .style(theme::ghost)
            .padding(10),
    ]
    .spacing(10)
    .align_y(Alignment::Center);

    // What the scan knows about it, in one muted line.
    let mut status = drive_status(app);
    if busy {
        status.push_str(" · backing up, so it is fixed until the run stops");
    }
    let status = text(status).size(CAPTION).color(MUTED);

    card("Choose a disc", column![field, status].spacing(12).into())
}

/// A field that is present but not editable, drawn as the input it replaces so
/// the card does not change shape when a run starts.
fn disabled_field<'a>(value: String) -> Element<'a, Message> {
    container(text(value).size(BODY).color(MUTED))
        .style(theme::field_disabled)
        .padding(10)
        .width(Fill)
        .into()
}

/// The device an eject would act on: the one this window read its disc from.
///
/// Taken from the disc the window actually loaded rather than from the scan's
/// `has_disc`. That comes from `/sys/block`, which on a drive the kernel is not
/// polling keeps saying "no disc" long after one is in — the very thing that
/// made detection miss discs — and a button greyed out by a stale answer cannot
/// eject the disc it is describing. A disc read from a folder has no device, so
/// there is nothing to eject and the button says so by being unavailable.
pub(crate) fn ejectable_device(app: &App) -> Option<PathBuf> {
    app.device.clone()
}

/// What the scan knows about the drive, in one muted line.
fn drive_status(app: &App) -> String {
    if let Some(drive) = app.selected_drive.and_then(|i| app.drives.get(i)) {
        let device = drive.device.display();
        return match (&drive.mount, drive.has_disc) {
            (Some(mount), _) => format!("{device} · mounted at {}", mount.display()),
            (None, true) => format!("{device} · disc inserted, read directly from the drive"),
            // The window is watching it, so an empty drive is a wait rather
            // than something to act on.
            (None, false) => format!("{device} · waiting for a disc"),
        };
    }
    if app.drives.is_empty() {
        return "No optical drives detected. Open a folder holding a disc instead.".to_string();
    }
    if app.disc.is_some() {
        return "Opened from a folder rather than a drive.".to_string();
    }
    format!("{} drives detected.", app.drives.len())
}

/// A drive as it appears in the dropdown.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DriveChoice {
    index: usize,
    label: String,
}

impl DriveChoice {
    fn new(index: usize, drive: &OpticalDrive) -> Self {
        let state = if drive.has_disc { "disc" } else { "empty" };
        let label = match drive.label() {
            Some(name) => format!("{} · {} ({state})", drive.device.display(), name),
            None => format!("{} ({state})", drive.device.display()),
        };
        Self { index, label }
    }
}

impl std::fmt::Display for DriveChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.label)
    }
}

/// One entry in the disc list: a drive the scan found, or a folder the user
/// opened by hand.
///
/// They share a control because they are the same kind of thing to the person
/// using it — somewhere a disc is — and because a separate field for the second
/// kind meant keeping two answers to one question.
#[derive(Debug, Clone, PartialEq, Eq)]
enum DiscChoice {
    Drive(DriveChoice),
    Folder(PathBuf),
}

impl std::fmt::Display for DiscChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Drive(drive) => f.write_str(&drive.label),
            Self::Folder(path) => write!(f, "{} · folder", path.display()),
        }
    }
}

/// Everything the disc can be chosen from, drives first.
fn disc_choices(app: &App) -> Vec<DiscChoice> {
    let mut choices: Vec<DiscChoice> = drive_choices(app)
        .into_iter()
        .map(DiscChoice::Drive)
        .collect();
    choices.extend(app.disc_paths.iter().cloned().map(DiscChoice::Folder));
    choices
}

/// The labels the list shows and the one it marks current.
///
/// Split out because a closed dropdown renders only its selection, so a
/// snapshot cannot show what is in it.
#[cfg(test)]
pub(crate) fn disc_choice_labels(app: &App) -> (Vec<String>, Option<String>) {
    (
        disc_choices(app).iter().map(ToString::to_string).collect(),
        selected_choice(app).map(|choice| choice.to_string()),
    )
}

/// Which entry the list should show as current.
fn selected_choice(app: &App) -> Option<DiscChoice> {
    if let Some(index) = app.selected_drive {
        if let Some(drive) = drive_choices(app).into_iter().find(|c| c.index == index) {
            return Some(DiscChoice::Drive(drive));
        }
    }
    // A folder is current only while no drive is: the two are set from the same
    // place and the last one to load wins.
    (app.disc.is_some() && !app.manual_path.is_empty())
        .then(|| DiscChoice::Folder(PathBuf::from(&app.manual_path)))
}

/// The source picker: a drive, or a folder.
fn section_label<'a>(label: &'a str) -> Element<'a, Message> {
    text(label).size(CAPTION).color(MUTED).into()
}

fn drive_choices(app: &App) -> Vec<DriveChoice> {
    app.drives
        .iter()
        .enumerate()
        .map(|(index, drive)| DriveChoice::new(index, drive))
        .collect()
}

/// Every title on the disc and what will be made of it.
///
/// One table rather than one for the tracks and another for the selection: a row
/// is a title, and the checkbox on it is the selection, so nothing has to be
/// read across two lists to find out what will happen. Titles nothing will be
/// made from are shown too, marked as such, because a menu or an alternate is
/// still part of what is on the disc.
fn titles_card(app: &App) -> Element<'_, Message> {
    let Some(disc) = &app.disc else {
        return card("Titles", empty("Nothing read yet."));
    };

    // In disc order, so the table reads like the disc does.
    let editable = !app.ripping();
    let mut rows: Vec<Element<'_, Message>> = Vec::new();
    for title in &disc.titles {
        let from_title: Vec<(usize, &Job)> = app
            .jobs
            .iter()
            .enumerate()
            .filter(|(_, job)| job.title == title.number)
            .collect();

        if from_title.is_empty() {
            rows.push(skipped_row(title, rows.len()));
        } else {
            for (index, job) in from_title {
                rows.push(job_row(index, job, title, rows.len(), editable));
            }
        }
    }

    let list: Element<'_, Message> = if rows.is_empty() {
        empty("This disc reports no titles.")
    } else {
        column![table_header(), divider(), column(rows).spacing(2)]
            .spacing(10)
            .into()
    };

    // The selection controls sit under the table they act on, rather than in
    // the action bar at the far end of the window. They stay there during a run
    // too, greyed, so the card does not change shape under the pointer.
    let content = column![list, selection_controls(app)].spacing(12);

    card("Titles", content.into())
}

/// How a row is banded, so a table is readable across its width.
fn stripe(stripe: usize) -> fn(&Theme) -> container::Style {
    if stripe % 2 == 1 {
        theme::row_alt
    } else {
        theme::row
    }
}

/// The heading of the one table, whose columns carry both the title and the
/// file made from it.
fn table_header() -> Element<'static, Message> {
    row![
        text("").width(Length::Fixed(34.0)),
        text("T#")
            .size(CAPTION)
            .color(MUTED)
            .width(Length::Fixed(46.0)),
        text("CHAPTERS")
            .size(CAPTION)
            .color(MUTED)
            .width(Length::Fixed(110.0)),
        text("LENGTH")
            .size(CAPTION)
            .color(MUTED)
            .width(Length::Fixed(64.0)),
        text("ANGLES")
            .size(CAPTION)
            .color(MUTED)
            .width(Length::Fixed(60.0)),
        text("VTS")
            .size(CAPTION)
            .color(MUTED)
            .width(Length::Fixed(50.0)),
        text("STATUS").size(CAPTION).color(MUTED).width(Fill),
    ]
    .spacing(12)
    .into()
}

/// A file that will be written, and the title it comes from.
fn job_row<'a>(
    index: usize,
    job: &'a Job,
    title: &'a Title,
    stripe_index: usize,
    editable: bool,
) -> Element<'a, Message> {
    // The output path gets its own full-width line: it is the longest and most
    // useful field, and a fixed column would clip it to nothing.
    container(
        column![
            row![
                container(
                    checkbox(job.enabled)
                        .style(theme::check)
                        .on_toggle_maybe(editable.then_some(move |_| Message::ToggleJob(index)))
                        .size(16)
                )
                .width(Length::Fixed(34.0)),
                text(format!("T{:02}", job.title))
                    .size(BODY)
                    .width(Length::Fixed(46.0)),
                text(chapter_span(job))
                    .size(BODY)
                    .color(MUTED)
                    .width(Length::Fixed(110.0)),
                text(jobs::fmt_duration(Some(job.duration)))
                    .size(BODY)
                    .color(MUTED)
                    .width(Length::Fixed(64.0)),
                text(title.angles.to_string())
                    .size(BODY)
                    .color(MUTED)
                    .width(Length::Fixed(60.0)),
                text(title.vts.to_string())
                    .size(BODY)
                    .color(MUTED)
                    .width(Length::Fixed(50.0)),
                status_cell(job),
            ]
            .spacing(12)
            .align_y(Alignment::Center),
            text(job.path.display().to_string())
                .size(CAPTION)
                .color(MUTED),
        ]
        .spacing(5),
    )
    .style(stripe(stripe_index))
    .padding(10)
    .width(Fill)
    .into()
}

/// A title nothing will be made from: a menu, an alternate, a stray track.
fn skipped_row(title: &Title, stripe_index: usize) -> Element<'static, Message> {
    container(
        row![
            text("").width(Length::Fixed(34.0)),
            text(format!("T{:02}", title.number))
                .size(BODY)
                .color(MUTED)
                .width(Length::Fixed(46.0)),
            text(format!("{} chapters", title.chapters))
                .size(BODY)
                .color(MUTED)
                .width(Length::Fixed(110.0)),
            text(jobs::fmt_duration(title.duration))
                .size(BODY)
                .color(MUTED)
                .width(Length::Fixed(64.0)),
            text(title.angles.to_string())
                .size(BODY)
                .color(MUTED)
                .width(Length::Fixed(60.0)),
            text(title.vts.to_string())
                .size(BODY)
                .color(MUTED)
                .width(Length::Fixed(50.0)),
            text("Not planned").size(CAPTION).color(WARNING).width(Fill),
        ]
        .spacing(12)
        .align_y(Alignment::Center),
    )
    .style(stripe(stripe_index))
    .padding(10)
    .width(Fill)
    .into()
}

/// `chapters 3-4`, or `chapter 3` when a file is one chapter long.
fn chapter_span(job: &Job) -> String {
    if job.first == job.last {
        format!("chapter {}", job.first)
    } else {
        format!("chapters {}-{}", job.first, job.last)
    }
}

/// The selection controls, under the table they act on.
///
/// Backing up is not here: that button is in the top bar, where it is in the
/// same place whatever the page is scrolled to.
fn selection_controls(app: &App) -> Element<'_, Message> {
    let selected = app.jobs.iter().filter(|job| job.enabled).count();
    let editable = !app.ripping();
    let extras = if app.include_extras {
        "Extras on"
    } else {
        "Extras off"
    };

    row![
        button(text(extras).size(BODY))
            .on_press_maybe(editable.then_some(Message::ToggleExtras))
            .style(theme::ghost)
            .padding(9),
        button(text("All").size(BODY))
            .on_press_maybe(editable.then_some(Message::SelectAll(true)))
            .style(theme::ghost)
            .padding(9),
        button(text("None").size(BODY))
            .on_press_maybe(editable.then_some(Message::SelectAll(false)))
            .style(theme::ghost)
            .padding(9),
        text("").width(Fill),
        text(format!("{selected} of {} selected", app.jobs.len()))
            .size(CAPTION)
            .color(MUTED),
        text(format!(
            "{} queued",
            jobs::fmt_duration(Some(jobs::remaining_duration(&app.jobs)))
        ))
        .size(CAPTION)
        .color(MUTED),
    ]
    .spacing(10)
    .align_y(Alignment::Center)
    .into()
}

/// The movie/TV verdict, with the choice left in the user's hands.
///
/// The heuristics read durations, chapter structure and the volume label, and
/// none of those can tell a short film from an episode, or a film whose chapters
/// repeat a rhythm from a "Play All" of episodes. Letting the verdict be clicked
/// is what keeps a wrong guess from being unfixable.
fn kind_picker(app: &App) -> Element<'_, Message> {
    let detected = app.classification.as_ref().map(|c| c.kind);
    let chosen = app.disc_kind();

    let mut children: Vec<Element<'_, Message>> =
        vec![text("Type").size(CAPTION).color(MUTED).into()];
    for (label, value) in [
        ("Movie", DiscKind::Movie),
        ("TV series", DiscKind::TvSeries),
    ] {
        let control = button(text(label).size(CAPTION)).padding([4, 10]);
        let control = if chosen == Some(value) {
            control.style(theme::primary)
        } else {
            control.style(theme::ghost)
        };
        // Re-planning a disc mid-run would rewrite the rows the run is working
        // from, so the choice is only live while there is no run.
        let control = if app.ripping() {
            control
        } else {
            control.on_press(Message::KindChosen(value))
        };
        children.push(control.into());
    }

    // How sure the reading was only means something while that reading is the
    // one in force; once it has been overruled, say so instead.
    if detected != chosen {
        children.push(badge("chosen by hand".to_string(), WARNING));
    } else {
        children.push(badge(
            match app.classification.as_ref() {
                Some(classification) => format!("{}% sure", classification.confidence),
                None => "not classified".to_string(),
            },
            MUTED,
        ));
    }

    row(children).spacing(8).align_y(Alignment::Center).into()
}

/// What the disc is, and what to call it.
///
/// One card, because they are one question. The identity the disc was read as
/// sits directly over the fields that correct it, and the label the disc
/// carries is already what those fields hold — printing it as a fact as well
/// would be saying the same thing twice.
fn disc_card(app: &App) -> Element<'_, Message> {
    let Some(disc) = &app.disc else {
        return card("Disc", empty("No disc loaded."));
    };

    let identity = row![
        text(disc.volume_id.clone()).size(HEADING).width(Fill),
        kind_picker(app),
    ]
    .spacing(12)
    .align_y(Alignment::Center);

    let facts = text(format!(
        "{} title{} · {}",
        disc.titles.len(),
        if disc.titles.len() == 1 { "" } else { "s" },
        jobs::fmt_duration(Some(disc.total_duration()))
    ))
    .size(CAPTION)
    .color(MUTED);

    let mut body = column![identity, facts, divider(), match_fields(app)].spacing(12);
    if let Some(note) = &app.css_note {
        body = body.push(notice_block(note));
    }

    card("", body.into())
}

/// The fields that decide what the disc is called.
///
/// While a run is going they are all still here, greyed out: the names it is
/// writing files with are the ones it started with, so changing them now would
/// only make the screen disagree with the files.
fn match_fields(app: &App) -> Element<'_, Message> {
    let mut body = column![].spacing(12);
    let editable = !app.ripping();

    if app.is_movie() {
        body = body.push(title_field(app, "Title", "The Matrix", editable));
        body = body.push(labelled_input(
            "Year",
            "1999",
            &app.season_input,
            Message::SeasonChanged,
            Fill,
            editable,
        ));
    } else {
        body = body.push(title_field(app, "Show", "Dragon Ball", editable));
        body = body.push(
            row![
                labelled_input(
                    "Season",
                    "1",
                    &app.season_input,
                    Message::SeasonChanged,
                    Length::Fixed(80.0),
                    editable,
                ),
                labelled_input(
                    "First episode",
                    "auto",
                    &app.first_episode_input,
                    Message::FirstEpisodeChanged,
                    Length::Fixed(110.0),
                    editable,
                ),
            ]
            .spacing(10),
        );
    }

    // Everything worth choosing between, whether it came from the search box or
    // from the resolver's own ranking. One list, because they are the same
    // question: which of these is the disc?
    if !app.title_matches.is_empty() {
        body = body.push(matches_list(app, editable));
    }

    body = body.push(
        button(text("Look up metadata").size(BODY))
            .on_press_maybe(editable.then_some(Message::RefreshMetadata))
            .style(theme::ghost)
            .padding(10),
    );

    body = body.push(match &app.search_state {
        MetaState::Loading => text("Searching…").size(CAPTION).color(MUTED),
        MetaState::Failed(message) => text(message.clone()).size(CAPTION).color(DANGER),
        MetaState::Idle => text(metadata_line(app)).size(CAPTION).color(MUTED),
    });

    body.into()
}

/// The title or show field, with the search that fills it beside it.
///
/// The field is the query: it already holds what the disc calls itself, so a
/// search is one press rather than a retype.
fn title_field<'a>(
    app: &'a App,
    label: &'a str,
    placeholder: &'a str,
    enabled: bool,
) -> Element<'a, Message> {
    column![
        section_label(label),
        row![
            text_input(placeholder, &app.show_input)
                .on_input_maybe(enabled.then_some(Message::ShowChanged))
                .on_submit_maybe(enabled.then_some(Message::RefreshMetadata))
                .style(theme::input)
                .padding(10)
                .width(Fill),
            button(text("Search").size(BODY))
                .on_press_maybe(enabled.then_some(Message::SearchTitles))
                .style(theme::ghost)
                .padding(10),
        ]
        .spacing(10)
        .align_y(Alignment::Center),
    ]
    .spacing(5)
    .into()
}

/// What the search found, as things to click.
fn matches_list(app: &App, enabled: bool) -> Element<'_, Message> {
    let mut list = column![].spacing(4);
    for (index, found) in app.title_matches.iter().enumerate() {
        let mut content = row![text(found.label.clone()).size(CAPTION).width(Fill)]
            .spacing(8)
            .align_y(Alignment::Center);
        if let Some(score) = found.score {
            content = content.push(
                text(format!("{:.0}%", score * 100.0))
                    .size(CAPTION)
                    .color(MUTED),
            );
        }
        list = list.push(
            button(content)
                .on_press_maybe(enabled.then_some(Message::ChooseTitleMatch(index)))
                .style(theme::choice)
                .padding(9)
                .width(Fill),
        );
    }
    list.into()
}

fn metadata_line(app: &App) -> String {
    if let Some(movie) = &app.meta_movie {
        return format!(
            "Matched {} ({})  ·  tmdb {}",
            movie.title,
            movie
                .year()
                .map(|y| y.to_string())
                .unwrap_or_else(|| "?".into()),
            movie.id
        );
    }
    if let Some(show) = &app.meta_show {
        return format!(
            "Matched {} ({})  ·  tvmaze {}",
            show.name,
            show.year()
                .map(|y| y.to_string())
                .unwrap_or_else(|| "?".into()),
            show.id
        );
    }
    if app.movie_naming.is_some() || app.naming.is_some() {
        return "Naming files from metadata".to_string();
    }

    // With a destination set but no match, files still get named — from the
    // label. Only an unset destination is actually a problem.
    let destination = if app.is_movie() {
        app.movie_path()
    } else {
        app.tv_path()
    };
    if destination.is_some() {
        "No metadata match; files will be named from the disc label".to_string()
    } else {
        "Set a destination in settings to name files from metadata".to_string()
    }
}

fn status_cell(job: &Job) -> Element<'_, Message> {
    let (label, color) = match &job.status {
        JobStatus::Pending => ("Ready".to_string(), MUTED),
        JobStatus::Running => ("Ripping…".to_string(), ACCENT),
        JobStatus::Done(report) if report.unreadable_sectors > 0 => (
            format!("{} bad sectors", report.unreadable_sectors),
            WARNING,
        ),
        JobStatus::Done(_) => ("Done".to_string(), SUCCESS),
        JobStatus::Failed(message) => (format!("Failed: {message}"), DANGER),
        JobStatus::Cancelled => ("Stopped".to_string(), WARNING),
    };

    text(label).size(CAPTION).color(color).width(Fill).into()
}

// ---------------------------------------------------------------------------
// Ripping
// ---------------------------------------------------------------------------
