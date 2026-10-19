mod scholar_view;

use std::{cell::Cell, path::PathBuf, time::Duration};

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::{
    ActiveTheme, Disableable, Icon, IconName, Selectable, Sizable, StyledExt as _, Theme,
    WindowExt,
    button::{Button, ButtonVariant, ButtonVariants},
    checkbox::Checkbox,
    dialog::{DialogAction, DialogClose, DialogFooter},
    h_flex,
    input::{Input, InputEvent, InputState},
    notification::Notification,
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::library::{
    CollectionId, EXPORT_FILE, ImportOutcome, Library, Paper, PaperId, ScanReport, is_pdf,
};
use scholar_view::ScholarState;

/// How often to look for changes made outside the app (e.g. in a file manager).
const REFRESH_INTERVAL: Duration = Duration::from_secs(1);

/// Key context of the main view, for key bindings.
pub const KEY_CONTEXT: &str = "PaprApp";

actions!(papr2, [SelectAllPapers, CloseSidePanel]);

/// Which list is shown in the main panel.
#[derive(Clone, PartialEq, Eq)]
enum View {
    Library,
    Collection(CollectionId),
    /// Search Google Scholar and add papers from the results.
    Scholar,
}

/// Payload for dragging one or more papers onto a collection in the sidebar.
#[derive(Clone)]
struct DraggedPapers {
    ids: Vec<PaperId>,
    label: SharedString,
}

impl Render for DraggedPapers {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .gap_2()
            .px_3()
            .py_1p5()
            .max_w(px(320.))
            .rounded(cx.theme().radius)
            .bg(cx.theme().popover)
            .border_1()
            .border_color(cx.theme().border)
            .shadow_md()
            .text_sm()
            .text_color(cx.theme().popover_foreground)
            .child(Icon::new(IconName::FileText).small())
            .child(div().truncate().child(self.label.clone()))
    }
}

pub struct PaprApp {
    library: Library,
    view: View,
    /// Selected papers, in the order they were selected.
    selection: Vec<PaperId>,
    /// Where a Shift-click range starts.
    anchor: Option<PaperId>,
    /// Multi-select mode: clicks toggle papers instead of replacing the selection.
    select_mode: bool,
    focus_handle: FocusHandle,
    title_input: Entity<InputState>,
    search_input: Entity<InputState>,
    scholar: ScholarState,
    _subscriptions: Vec<Subscription>,
    dialog_subscription: Option<Subscription>,
    _refresh_task: Task<()>,
}

impl PaprApp {
    pub fn new(library: Library, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let title_input = cx.new(|cx| InputState::new(window, cx).placeholder("Paper name"));
        let search_input = cx.new(|cx| InputState::new(window, cx).placeholder("Search papers…"));
        let (scholar, scholar_subscription) = ScholarState::new(window, cx);

        // Follow the system's light/dark setting, now and when it changes.
        // (On Linux the window knows the appearance; the app-level value
        // may not be available yet at startup.)
        Theme::sync_system_appearance(Some(window), cx);

        let subscriptions = vec![
            cx.observe_window_appearance(window, |_, window, cx| {
                Theme::sync_system_appearance(Some(window), cx);
            }),
            cx.subscribe_in(&title_input, window, |this, _, ev: &InputEvent, window, cx| {
                match ev {
                    InputEvent::PressEnter { .. } | InputEvent::Blur => {
                        this.commit_title(window, cx)
                    }
                    _ => {}
                }
            }),
            cx.subscribe_in(&search_input, window, |_, _, ev: &InputEvent, _, cx| {
                if let InputEvent::Change = ev {
                    cx.notify();
                }
            }),
            scholar_subscription,
        ];

        // Poll the library folder so edits made in a file manager show up.
        let refresh_task = cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(REFRESH_INTERVAL).await;
                let alive = this
                    .update_in(cx, |this, window, cx| this.refresh(window, cx))
                    .is_ok();
                if !alive {
                    break;
                }
            }
        });

        // Focus the main view so keyboard shortcuts work straight away.
        let focus_handle = cx.focus_handle();
        focus_handle.focus(window, cx);

        Self {
            library,
            view: View::Library,
            selection: Vec::new(),
            anchor: None,
            select_mode: false,
            focus_handle,
            title_input,
            search_input,
            scholar,
            _subscriptions: subscriptions,
            dialog_subscription: None,
            _refresh_task: refresh_task,
        }
    }

    // ----- helpers ----------------------------------------------------------

    fn notify_error(window: &mut Window, cx: &mut App, err: anyhow::Error) {
        window.push_notification(Notification::error(format!("{err:#}")), cx);
    }

    /// The paper shown in the details pane, if exactly one is selected
    /// outside of multi-select mode.
    fn single_selection(&self) -> Option<&PaperId> {
        match self.selection.as_slice() {
            [id] if !self.select_mode => Some(id),
            _ => None,
        }
    }

    /// Re-scan the library folder and follow any renames.
    fn refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.library.refresh() {
            Ok(report) => self.apply_report(report, window, cx),
            Err(err) => eprintln!("papr2: refresh failed: {err:#}"),
        }
    }

    /// Keep the selection and current view pointing at the right things after
    /// the library changed (possibly behind our back).
    fn apply_report(&mut self, report: ScanReport, window: &mut Window, cx: &mut Context<Self>) {
        for (old, new) in &report.renamed_collections {
            if self.view == View::Collection(old.clone()) {
                self.view = View::Collection(new.clone());
            }
        }
        if let View::Collection(cid) = &self.view {
            if self.library.collection(cid).is_none() {
                self.view = View::Library;
            }
        }

        let follow = |id: &PaperId| -> PaperId {
            report
                .renamed_papers
                .iter()
                .find(|(old, _)| old == id)
                .map(|(_, new)| new.clone())
                .unwrap_or_else(|| id.clone())
        };
        let selection: Vec<PaperId> = self
            .selection
            .iter()
            .map(follow)
            .filter(|id| self.library.paper(id).is_some())
            .collect();
        self.anchor = self
            .anchor
            .as_ref()
            .map(follow)
            .filter(|id| self.library.paper(id).is_some());
        self.scholar.follow_renames(&report.renamed_papers);
        if selection != self.selection {
            let removed = selection.len() < self.selection.len();
            self.selection = selection;
            self.sync_title_input(removed, window, cx);
            cx.notify();
        }

        if report.changed || !report.renamed_collections.is_empty() {
            cx.notify();
        }
    }

    /// Show the selected paper's name in the name field. Unless `force`d,
    /// leaves the field alone while the user is typing in it.
    fn sync_title_input(&mut self, force: bool, window: &mut Window, cx: &mut Context<Self>) {
        if !force && self.title_input.read(cx).focus_handle(cx).is_focused(window) {
            return;
        }
        let title = match self.selection.as_slice() {
            [id] => self.library.paper(id).map(|p| p.title().to_string()),
            _ => None,
        }
        .unwrap_or_default();
        self.title_input
            .update(cx, |input, cx| input.set_value(title, window, cx));
    }

    /// Papers for the current view, filtered by the search box and sorted by name.
    fn visible_papers(&self, cx: &App) -> Vec<Paper> {
        let query = self.search_input.read(cx).value().trim().to_lowercase();
        let mut papers: Vec<Paper> = match &self.view {
            View::Library | View::Scholar => self.library.papers().to_vec(),
            View::Collection(cid) => self
                .library
                .collection(cid)
                .map(|c| {
                    c.papers
                        .iter()
                        .filter_map(|id| self.library.paper(id).cloned())
                        .collect()
                })
                .unwrap_or_default(),
        };
        if !query.is_empty() {
            papers.retain(|p| {
                p.title().to_lowercase().contains(&query)
                    || p.info().is_some_and(|i| {
                        i.authors.iter().any(|a| a.to_lowercase().contains(&query))
                            || i.title.as_ref().is_some_and(|t| t.to_lowercase().contains(&query))
                    })
            });
        }
        papers.sort_by_key(|p| p.title().to_lowercase());
        papers
    }

    fn set_view(&mut self, view: View, window: &mut Window, cx: &mut Context<Self>) {
        // Keep only selected papers that are still visible.
        match &view {
            View::Collection(cid) => {
                let members = self
                    .library
                    .collection(cid)
                    .map(|c| c.papers.clone())
                    .unwrap_or_default();
                let selection = self
                    .selection
                    .iter()
                    .filter(|id| members.contains(id))
                    .cloned()
                    .collect();
                self.set_selection(selection, window, cx);
            }
            View::Scholar => {
                self.select_mode = false;
                self.set_selection(Vec::new(), window, cx);
            }
            View::Library => {}
        }
        self.view = view;
        cx.notify();
    }

    // ----- selection ------------------------------------------------------------

    fn set_selection(&mut self, ids: Vec<PaperId>, window: &mut Window, cx: &mut Context<Self>) {
        if ids == self.selection {
            return;
        }
        // Don't lose an in-progress rename when the selection changes.
        self.commit_title(window, cx);
        self.selection = ids;
        self.sync_title_input(true, window, cx);
        cx.notify();
    }

    fn select_paper(&mut self, id: PaperId, window: &mut Window, cx: &mut Context<Self>) {
        self.anchor = Some(id.clone());
        self.set_selection(vec![id], window, cx);
    }

    fn toggle_paper(&mut self, id: &PaperId, window: &mut Window, cx: &mut Context<Self>) {
        let mut selection = self.selection.clone();
        match selection.iter().position(|s| s == id) {
            Some(ix) => {
                selection.remove(ix);
            }
            None => selection.push(id.clone()),
        }
        self.anchor = Some(id.clone());
        self.set_selection(selection, window, cx);
    }

    /// Plain click selects one paper; Ctrl/Cmd-click (or any click in
    /// multi-select mode) toggles; Shift-click selects a range.
    fn click_paper(
        &mut self,
        id: &PaperId,
        event: &ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.focus_handle.focus(window, cx);
        let modifiers = event.modifiers();
        let additive = modifiers.secondary() || self.select_mode;

        if modifiers.shift {
            if let Some(anchor) = self.anchor.clone() {
                let visible: Vec<PaperId> =
                    self.visible_papers(cx).into_iter().map(|p| p.id).collect();
                let from = visible.iter().position(|p| *p == anchor);
                let to = visible.iter().position(|p| p == id);
                if let (Some(from), Some(to)) = (from, to) {
                    let mut range = visible[from.min(to)..=from.max(to)].to_vec();
                    if additive {
                        for selected in &self.selection {
                            if !range.contains(selected) {
                                range.push(selected.clone());
                            }
                        }
                    }
                    self.set_selection(range, window, cx);
                    return;
                }
            }
        }

        if additive {
            self.toggle_paper(id, window, cx);
        } else {
            self.select_paper(id.clone(), window, cx);
            if event.click_count() == 2 {
                self.open_paper(id, window, cx);
            }
        }
    }

    fn select_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.view == View::Scholar {
            return;
        }
        let all = self.visible_papers(cx).into_iter().map(|p| p.id).collect();
        self.set_selection(all, window, cx);
    }

    /// Clears the selection and leaves multi-select mode, closing the right pane.
    fn close_side_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.view == View::Scholar {
            self.scholar.close_result();
        }
        self.select_mode = false;
        self.anchor = None;
        self.set_selection(Vec::new(), window, cx);
        cx.notify();
    }

    fn toggle_select_mode(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.select_mode {
            self.close_side_panel(window, cx);
        } else {
            self.commit_title(window, cx);
            self.select_mode = true;
            cx.notify();
        }
    }

    fn on_select_all(&mut self, _: &SelectAllPapers, window: &mut Window, cx: &mut Context<Self>) {
        self.select_all(window, cx);
    }

    fn on_close_side_panel(
        &mut self,
        _: &CloseSidePanel,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_side_panel(window, cx);
    }

    fn commit_title(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let [id] = self.selection.as_slice() else { return };
        let id = id.clone();
        let Some(current) = self.library.paper(&id).map(|p| p.title().to_string()) else {
            return;
        };
        let value = self.title_input.read(cx).value().trim().to_string();
        if value == current {
            return;
        }
        if value.is_empty() {
            self.sync_title_input(true, window, cx);
            return;
        }
        match self.library.rename_paper(&id, &value) {
            Ok(new_id) => {
                self.anchor = Some(new_id.clone());
                self.selection = vec![new_id];
            }
            Err(err) => {
                Self::notify_error(window, cx, err);
                self.sync_title_input(true, window, cx);
            }
        }
        cx.notify();
    }

    // ----- importing ----------------------------------------------------------

    fn prompt_import(&mut self, _: &ClickEvent, window: &mut Window, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: true,
            multiple: true,
            prompt: Some("Import".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await {
                this.update_in(cx, |this, window, cx| {
                    this.import_paths(&paths, None, window, cx)
                })
                .ok();
            }
        })
        .detach();
    }

    /// Files dropped on the window: in the Scholar view they become the PDF
    /// of the paper just added; elsewhere they're imported.
    fn drop_files(&mut self, paths: &ExternalPaths, window: &mut Window, cx: &mut Context<Self>) {
        if self.view == View::Scholar {
            if let Some(id) = self.scholar.paper_awaiting_pdf(&self.library) {
                self.attach_pdf_from(&id, paths.paths(), window, cx);
                return;
            }
        }
        self.import_paths(paths.paths(), None, window, cx)
    }

    /// Import PDFs (directories are searched recursively). Imported papers are
    /// also added to `into` or, if that's `None`, to the collection currently
    /// being viewed.
    fn import_paths(
        &mut self,
        paths: &[PathBuf],
        into: Option<CollectionId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let target = into.or(match &self.view {
            View::Collection(cid) => Some(cid.clone()),
            View::Library | View::Scholar => None,
        });

        let mut pdfs = Vec::new();
        for path in paths {
            collect_pdfs(path, &mut pdfs);
        }
        if pdfs.is_empty() {
            window.push_notification(Notification::warning("No PDF files found to import"), cx);
            return;
        }

        let (mut added, mut duplicates, mut last) = (0, 0, None);
        for pdf in &pdfs {
            match self.library.import_pdf(pdf) {
                Ok(outcome) => {
                    match &outcome {
                        ImportOutcome::Added(_) => added += 1,
                        ImportOutcome::Duplicate(_) => duplicates += 1,
                    }
                    let id = outcome.id();
                    if let Some(cid) = &target {
                        if let Err(err) = self.library.add_to_collection(cid, &id) {
                            Self::notify_error(window, cx, err);
                        }
                    }
                    last = Some(id);
                }
                Err(err) => Self::notify_error(window, cx, err),
            }
        }

        let mut msg = format!("Imported {}", count_papers(added));
        if duplicates > 0 {
            msg.push_str(&format!(" ({duplicates} already in library)"));
        }
        if let Some(cid) = &target {
            msg.push_str(&format!(" into “{cid}”"));
        }
        window.push_notification(Notification::success(msg), cx);

        if let Some(id) = last {
            if self.view == View::Scholar {
                self.view = View::Library;
            }
            self.select_mode = false;
            self.select_paper(id, window, cx);
        }
        cx.notify();
    }

    // ----- paper actions --------------------------------------------------------

    fn open_paper(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        match self.library.pdf_path(id) {
            Some(path) if path.exists() => cx.open_with_system(&path),
            Some(path) => window.push_notification(
                Notification::error(format!("The file {} is missing", path.display())),
                cx,
            ),
            None => window.push_notification(
                Notification::warning(format!("“{id}” has no PDF yet. Attach one first.")),
                cx,
            ),
        }
    }

    /// Attach (or replace) a paper's PDF from the first PDF in `paths`.
    fn attach_pdf_from(
        &mut self,
        id: &str,
        paths: &[PathBuf],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(pdf) = paths.iter().find(|p| is_pdf(p)) else {
            window.push_notification(Notification::warning("That isn't a PDF file"), cx);
            return;
        };
        let replacing = self.library.paper(id).is_some_and(|p| p.pdf.is_some());
        match self.library.attach_pdf(id, pdf) {
            Ok(()) => window.push_notification(
                Notification::success(if replacing {
                    format!("Replaced the PDF of “{id}”")
                } else {
                    format!("Attached the PDF to “{id}”")
                }),
                cx,
            ),
            Err(err) => Self::notify_error(window, cx, err),
        }
        cx.notify();
    }

    /// Asks for a PDF file, then attaches it to (or replaces it in) a paper.
    fn prompt_attach_pdf(&mut self, id: PaperId, window: &mut Window, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Attach PDF".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await {
                this.update_in(cx, |this, window, cx| {
                    this.attach_pdf_from(&id, &paths, window, cx)
                })
                .ok();
            }
        })
        .detach();
    }

    /// PDFs dropped onto the details pane: attach, or confirm replacing.
    fn drop_pdf_on_paper(
        &mut self,
        id: PaperId,
        paths: Vec<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.library.paper(&id).is_some_and(|p| p.pdf.is_some()) {
            self.attach_pdf_from(&id, &paths, window, cx);
            return;
        }
        let this = cx.entity().downgrade();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let (this, id, paths) = (this.clone(), id.clone(), paths.clone());
            alert
                .title(format!("Replace the PDF of “{id}”?"))
                .description("The current PDF will be overwritten with the one you dropped.")
                .show_cancel(true)
                .ok_text("Replace")
                .on_ok(move |_, window, cx| {
                    this.update(cx, |this, cx| this.attach_pdf_from(&id, &paths, window, cx))
                        .ok();
                    true
                })
        });
    }

    fn copy_bibtex(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        match self.library.read_bib(id) {
            Some(text) => {
                cx.write_to_clipboard(ClipboardItem::new_string(text));
                window.push_notification(Notification::success("Copied the BibTeX entry"), cx);
            }
            None => window.push_notification(Notification::error("Couldn't read the .bib file"), cx),
        }
    }

    fn confirm_delete_papers(
        &mut self,
        ids: Vec<PaperId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (title, description) = match ids.as_slice() {
            [] => return,
            [id] => {
                if self.library.paper(id).is_none() {
                    return;
                }
                let n = self.library.collections_of(id).count();
                let mut description = format!(
                    "{} will be deleted from disk.",
                    self.library.relative_paths(id).join(" and ")
                );
                if n > 0 {
                    description.push_str(&format!(
                        " It will also be removed from {n} collection{}.",
                        if n == 1 { "" } else { "s" }
                    ));
                }
                (format!("Delete “{id}”?"), description)
            }
            ids => (
                format!("Delete {}?", count_papers(ids.len())),
                "Their PDF and BibTeX files will be deleted from Library/ and removed from every collection."
                    .to_string(),
            ),
        };

        let this = cx.entity().downgrade();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let this = this.clone();
            let ids = ids.clone();
            alert
                .title(title.clone())
                .description(description.clone())
                .show_cancel(true)
                .ok_text("Delete")
                .ok_variant(ButtonVariant::Danger)
                .on_ok(move |_, window, cx| {
                    this.update(cx, |this, cx| {
                        let selection = this
                            .selection
                            .iter()
                            .filter(|id| !ids.contains(id))
                            .cloned()
                            .collect();
                        this.set_selection(selection, window, cx);
                        for id in &ids {
                            if let Err(err) = this.library.delete_paper(id) {
                                Self::notify_error(window, cx, err);
                                break;
                            }
                        }
                        cx.notify();
                    })
                    .ok();
                    true
                })
        });
    }

    fn set_membership(
        &mut self,
        cid: &str,
        pid: &str,
        member: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if member {
            self.add_papers_to_collection(cid, &[pid.to_string()], false, window, cx);
        } else {
            self.remove_papers_from_collection(cid, &[pid.to_string()], false, window, cx);
        }
    }

    /// Adds papers to a collection (as links; nothing is copied).
    fn add_papers_to_collection(
        &mut self,
        cid: &str,
        ids: &[PaperId],
        announce: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut added = 0;
        for id in ids {
            match self.library.add_to_collection(cid, id) {
                Ok(true) => added += 1,
                Ok(false) => {}
                Err(err) => {
                    Self::notify_error(window, cx, err);
                    break;
                }
            }
        }
        if announce {
            let note = match (added, ids) {
                (0, [id]) => Notification::info(format!("“{id}” is already in “{cid}”")),
                (0, _) => Notification::info(format!("These papers are already in “{cid}”")),
                (_, [id]) => Notification::success(format!("Added “{id}” to “{cid}”")),
                (n, _) => Notification::success(format!("Added {} to “{cid}”", count_papers(n))),
            };
            window.push_notification(note, cx);
        }
        cx.notify();
    }

    fn remove_papers_from_collection(
        &mut self,
        cid: &str,
        ids: &[PaperId],
        announce: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        for id in ids {
            if let Err(err) = self.library.remove_from_collection(cid, id) {
                Self::notify_error(window, cx, err);
                break;
            }
        }
        // Papers removed from the collection being viewed disappear from the list.
        if self.view == View::Collection(cid.to_string()) {
            let selection = self
                .selection
                .iter()
                .filter(|id| !ids.contains(id))
                .cloned()
                .collect();
            self.set_selection(selection, window, cx);
        }
        if announce {
            window.push_notification(
                Notification::success(format!(
                    "Removed {} from “{cid}”",
                    count_papers(ids.len())
                )),
                cx,
            );
        }
        cx.notify();
    }

    // ----- collection actions -----------------------------------------------------

    /// Opens a dialog to create a collection (`existing == None`) or rename one.
    /// A new collection is created containing the papers in `add`.
    fn open_collection_dialog(
        &mut self,
        existing: Option<CollectionId>,
        add: Vec<PaperId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let current = existing.clone().unwrap_or_default();
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("e.g. Thesis, Literature review…")
                .default_value(current)
        });

        // Pressing Enter in the name field submits the dialog.
        let (existing_enter, add_enter) = (existing.clone(), add.clone());
        self.dialog_subscription = Some(cx.subscribe_in(
            &input,
            window,
            move |this, input, ev: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = ev {
                    let name = input.read(cx).value().to_string();
                    let (existing, add) = (existing_enter.clone(), add_enter.clone());
                    if this.submit_collection_dialog(existing, add, &name, window, cx) {
                        window.close_dialog(cx);
                    }
                }
            },
        ));

        let this = cx.entity().downgrade();
        let first_build = Cell::new(true);
        let title = match (&existing, add.len()) {
            (Some(_), _) => "Rename collection".to_string(),
            (None, 0) => "New collection".to_string(),
            (None, n) => format!("New collection with {}", count_papers(n)),
        };
        let ok = if existing.is_some() { "Rename" } else { "Create" };
        window.open_dialog(cx, move |dialog, window, cx| {
            if first_build.replace(false) {
                input.focus_handle(cx).focus(window, cx);
            }
            let input_ok = input.clone();
            let this = this.clone();
            let (existing, add) = (existing.clone(), add.clone());
            dialog
                .title(title.clone())
                .w(px(420.))
                .child(Input::new(&input))
                .footer(
                    DialogFooter::new()
                        .child(DialogClose::new().trigger(|b| b.outline().label("Cancel")))
                        .child(DialogAction::new().child(Button::new("ok").primary().label(ok))),
                )
                .on_ok(move |_, window, cx| {
                    let name = input_ok.read(cx).value().to_string();
                    let (existing, add) = (existing.clone(), add.clone());
                    this.update(cx, |this, cx| {
                        this.submit_collection_dialog(existing, add, &name, window, cx)
                    })
                    .unwrap_or(true)
                })
        });
    }

    /// Returns `true` if the dialog should close.
    fn submit_collection_dialog(
        &mut self,
        existing: Option<CollectionId>,
        add: Vec<PaperId>,
        name: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if name.trim().is_empty() {
            return false;
        }
        let result = match &existing {
            Some(id) => self.library.rename_collection(id, name).map(|new_id| {
                if self.view == View::Collection(id.clone()) {
                    self.view = View::Collection(new_id);
                }
            }),
            None => self.library.create_collection(name).map(|new_id| {
                if add.is_empty() {
                    self.close_side_panel(window, cx);
                } else {
                    self.add_papers_to_collection(&new_id, &add, true, window, cx);
                }
                self.view = View::Collection(new_id);
            }),
        };
        cx.notify();
        match result {
            Ok(()) => {
                self.dialog_subscription = None;
                true
            }
            Err(err) => {
                Self::notify_error(window, cx, err);
                false
            }
        }
    }

    fn confirm_delete_collection(
        &mut self,
        cid: CollectionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let this = cx.entity().downgrade();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let this = this.clone();
            let cid = cid.clone();
            alert
                .title(format!("Delete collection “{cid}”?"))
                .description(format!(
                    "The folder Collections/{cid} will be removed. \
                     The papers stay in your Library."
                ))
                .show_cancel(true)
                .ok_text("Delete")
                .ok_variant(ButtonVariant::Danger)
                .on_ok(move |_, window, cx| {
                    let cid = cid.clone();
                    this.update(cx, |this, cx| {
                        if let Err(err) = this.library.delete_collection(&cid) {
                            Self::notify_error(window, cx, err);
                        }
                        if this.library.collection(&cid).is_none()
                            && this.view == View::Collection(cid)
                        {
                            this.view = View::Library;
                        }
                        cx.notify();
                    })
                    .ok();
                    true
                })
        });
    }

    /// Writes `Collections/<cid>/references.bib`.
    fn export_collection_bib(&mut self, cid: &str, window: &mut Window, cx: &mut Context<Self>) {
        match self.library.export_collection_bib(cid) {
            Ok(summary) => {
                let path = summary.path.strip_prefix(self.library.root()).unwrap_or(&summary.path);
                let mut msg = format!(
                    "Wrote {} with {} entr{}",
                    path.display(),
                    summary.entries,
                    if summary.entries == 1 { "y" } else { "ies" }
                );
                if !summary.missing.is_empty() {
                    msg.push_str(&format!(
                        ". {} without BibTeX {} left out.",
                        count_papers(summary.missing.len()),
                        if summary.missing.len() == 1 { "was" } else { "were" }
                    ));
                }
                window.push_notification(Notification::success(msg), cx);
            }
            Err(err) => Self::notify_error(window, cx, err),
        }
    }

    // ----- rendering ------------------------------------------------------------

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let library_count = self.library.papers().len();

        let nav_item = |id: &'static str, icon: IconName, label: &'static str, active: bool| {
            h_flex()
                .id(id)
                .gap_2()
                .px_2()
                .py_1p5()
                .rounded(theme.radius)
                .cursor_pointer()
                .hover(|s| s.bg(theme.sidebar_accent))
                .when(active, |s| {
                    s.bg(theme.sidebar_accent)
                        .text_color(theme.sidebar_accent_foreground)
                        .font_medium()
                })
                .child(Icon::new(icon).small())
                .child(div().flex_1().child(label))
        };
        let library_item = nav_item("nav-library", IconName::BookOpen, "My Library", self.view == View::Library)
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(library_count.to_string()),
            )
            .on_click(cx.listener(|this, _, window, cx| this.set_view(View::Library, window, cx)));
        let scholar_item = nav_item("nav-scholar", IconName::Search, "Google Scholar", self.view == View::Scholar)
            .on_click(cx.listener(|this, _, window, cx| this.set_view(View::Scholar, window, cx)));

        let collections = self.library.collections().iter().enumerate().map(|(ix, c)| {
            let cid = c.id.clone();
            let active = self.view == View::Collection(cid.clone());
            let group: SharedString = format!("collection-{ix}").into();
            h_flex()
                .id(ElementId::NamedInteger("collection".into(), ix as u64))
                .group(group.clone())
                .gap_2()
                .pl_2()
                .pr_1()
                .h(px(32.))
                .rounded(theme.radius)
                .border_1()
                .border_color(transparent_black())
                .cursor_pointer()
                .hover(|s| s.bg(theme.sidebar_accent))
                .when(active, |s| {
                    s.bg(theme.sidebar_accent)
                        .text_color(theme.sidebar_accent_foreground)
                        .font_medium()
                })
                .drag_over::<DraggedPapers>(|s, _, _, cx| {
                    s.bg(cx.theme().drop_target)
                        .border_color(cx.theme().drag_border)
                })
                .drag_over::<ExternalPaths>(|s, _, _, cx| {
                    s.bg(cx.theme().drop_target)
                        .border_color(cx.theme().drag_border)
                })
                .on_drop(cx.listener({
                    let cid = cid.clone();
                    move |this, papers: &DraggedPapers, window, cx| {
                        this.add_papers_to_collection(&cid, &papers.ids, true, window, cx)
                    }
                }))
                .on_drop(cx.listener({
                    let cid = cid.clone();
                    move |this, paths: &ExternalPaths, window, cx| {
                        this.import_paths(paths.paths(), Some(cid.clone()), window, cx)
                    }
                }))
                .on_click(cx.listener({
                    let cid = cid.clone();
                    move |this, _, window, cx| {
                        this.set_view(View::Collection(cid.clone()), window, cx)
                    }
                }))
                .child(
                    Icon::new(if active {
                        IconName::FolderOpen
                    } else {
                        IconName::Folder
                    })
                    .small(),
                )
                .child(div().flex_1().min_w_0().truncate().child(c.name().to_string()))
                // The paper count, swapped for rename/delete buttons on hover
                // (they take its place rather than drawing over it).
                .child(
                    div()
                        .id(ElementId::NamedInteger("col-count".into(), ix as u64))
                        .min_w(px(20.))
                        .text_right()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .group_hover(group.clone(), |s| s.hidden())
                        .child(c.papers.len().to_string())
                        .test_support(),
                )
                .child(
                    h_flex()
                        .id(ElementId::NamedInteger("col-actions".into(), ix as u64))
                        .hidden()
                        .group_hover(group.clone(), |s| s.flex())
                        .child(
                            Button::new(ElementId::NamedInteger("rename-col".into(), ix as u64))
                                .ghost()
                                .xsmall()
                                .icon(Lucide::Pencil)
                                .tooltip("Rename collection")
                                .on_click(cx.listener({
                                    let cid = cid.clone();
                                    move |this, _, window, cx| {
                                        cx.stop_propagation();
                                        this.open_collection_dialog(
                                            Some(cid.clone()),
                                            Vec::new(),
                                            window,
                                            cx,
                                        )
                                    }
                                })),
                        )
                        .child(
                            Button::new(ElementId::NamedInteger("delete-col".into(), ix as u64))
                                .ghost()
                                .xsmall()
                                .icon(Lucide::Trash)
                                .tooltip("Delete collection")
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    cx.stop_propagation();
                                    this.confirm_delete_collection(cid.clone(), window, cx)
                                })),
                        ),
                )
                .test_support()
        });

        v_flex()
            .id("sidebar")
            .w(px(230.))
            .h_full()
            .flex_shrink_0()
            .bg(theme.sidebar)
            .text_color(theme.sidebar_foreground)
            .border_r_1()
            .border_color(theme.sidebar_border)
            .text_sm()
            // Clicking anywhere in the sidebar closes the paper details pane.
            // (The Google Scholar pane keeps its draft; it has its own close button.)
            .capture_any_mouse_down(cx.listener(|this, _: &MouseDownEvent, window, cx| {
                if this.view != View::Scholar {
                    this.close_side_panel(window, cx)
                }
            }))
            .child(
                h_flex()
                    .gap_2()
                    .pl_4()
                    .pr_2()
                    .h(px(TOOLBAR_HEIGHT))
                    .flex_shrink_0()
                    .text_base()
                    .font_semibold()
                    .child(Icon::new(Lucide::LibraryBig))
                    .child(div().flex_1().child("papr2"))
                    .child(
                        Button::new("open-root")
                            .ghost()
                            .xsmall()
                            .icon(IconName::FolderOpen)
                            .tooltip("Open the papr2 folder")
                            .on_click(cx.listener(|this, _, _, cx| {
                                cx.open_with_system(this.library.root())
                            })),
                    ),
            )
            .child(v_flex().px_2().gap_0p5().child(library_item).child(scholar_item))
            .child(
                h_flex()
                    .mt_4()
                    .pl_4()
                    .pr_2()
                    .pb_1()
                    .justify_between()
                    .text_xs()
                    .font_medium()
                    .text_color(theme.muted_foreground)
                    .child("COLLECTIONS")
                    .child(
                        Button::new("new-collection")
                            .ghost()
                            .xsmall()
                            .icon(Lucide::FolderPlus)
                            .tooltip("New collection")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_collection_dialog(None, Vec::new(), window, cx)
                            })),
                    ),
            )
            .child(
                v_flex()
                    .id("collections")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px_2()
                    .gap_0p5()
                    .children(collections)
                    .when(self.library.collections().is_empty(), |s| {
                        s.child(
                            div()
                                .px_2()
                                .py_1()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child("No collections yet. Click + to create one."),
                        )
                    }),
            )
            .child(
                div()
                    .p_3()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child("Tip: drag papers onto a collection to add them."),
            )
    }

    fn render_paper_row(&self, ix: usize, paper: &Paper, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let id = paper.id.clone();
        let selected = self.selection.contains(&id);
        let info = paper.info();
        let collections: Vec<&str> = self.library.collections_of(&id).map(|c| c.name()).collect();

        let mut meta = Vec::new();
        if let Some(year) = info.and_then(|i| i.year.as_ref()) {
            meta.push(year.clone());
        }
        if let Some(pdf) = &paper.pdf {
            meta.push(format_size(pdf.size));
        }
        if !collections.is_empty() {
            meta.push(collections.join(", "));
        }

        let mut warnings = Vec::new();
        if paper.pdf.is_none() {
            warnings.push("No PDF");
        }
        match &paper.bib {
            None => warnings.push("No BibTeX"),
            Some(bib) if bib.error.is_some() => warnings.push("BibTeX unreadable"),
            Some(_) => {}
        }

        // Dragging a selected paper drags the whole selection.
        let drag = if selected && self.selection.len() > 1 {
            DraggedPapers {
                ids: self.selection.clone(),
                label: count_papers(self.selection.len()).into(),
            }
        } else {
            DraggedPapers {
                ids: vec![id.clone()],
                label: paper.title().to_string().into(),
            }
        };

        h_flex()
            .id(ElementId::NamedInteger("paper".into(), ix as u64))
            .items_start()
            .gap_3()
            .px_4()
            .py_2p5()
            .border_b_1()
            .border_color(theme.border)
            .cursor_pointer()
            .hover(|s| s.bg(theme.list_hover))
            .when(selected, |s| s.bg(theme.list_active))
            .on_click(cx.listener({
                let id = id.clone();
                move |this, ev: &ClickEvent, window, cx| this.click_paper(&id, ev, window, cx)
            }))
            .on_drag(drag, |drag, _, _, cx| cx.new(|_| drag.clone()))
            .when(self.select_mode, |row| {
                row.child(
                    // Display only: the row's click handler does the toggling.
                    // (Click events bubble to parents in GPUI, so giving the
                    // checkbox its own handler would toggle twice.)
                    div().pt_0p5().child(
                        Checkbox::new(ElementId::NamedInteger("pick".into(), ix as u64))
                            .checked(selected),
                    ),
                )
            })
            .child(
                div().pt_0p5().child(
                    Icon::new(if paper.pdf.is_some() {
                        IconName::FileText
                    } else {
                        IconName::File
                    })
                    .text_color(theme.muted_foreground),
                ),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_0p5()
                    .child(
                        h_flex()
                            .gap_2()
                            .child(div().min_w_0().truncate().font_medium().child(paper.title().to_string()))
                            .children(warnings.into_iter().map(|w| warning_badge(w, cx))),
                    )
                    .when_some(info.filter(|i| !i.authors.is_empty()), |col, info| {
                        col.child(
                            div()
                                .truncate()
                                .text_sm()
                                .child(info.authors.join(", ")),
                        )
                    })
                    .when_some(info.and_then(|i| i.abstract_.clone()), |col, abstract_| {
                        col.child(
                            div()
                                .text_xs()
                                // Without an ellipsis, gpui puts all remaining text on the last
                                // line and clips it mid-word.
                                .line_clamp(2)
                                .text_ellipsis()
                                .text_color(theme.muted_foreground)
                                .child(abstract_),
                        )
                    })
                    .when(!meta.is_empty(), |col| {
                        col.child(
                            div()
                                .truncate()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(meta.join(" · ")),
                        )
                    }),
            )
            .when(paper.pdf.is_some(), |row| {
                row.child(
                    Button::new(ElementId::NamedInteger("open".into(), ix as u64))
                        .ghost()
                        .small()
                        .icon(IconName::ExternalLink)
                        .tooltip("Open PDF")
                        .on_click(cx.listener(move |this, _, window, cx| {
                            // Don't also select/toggle the row.
                            cx.stop_propagation();
                            this.open_paper(&id, window, cx)
                        })),
                )
            })
            // Lets UI tests click rows; a no-op in normal builds.
            .test_support()
            .into_any_element()
    }

    fn render_main(&self, cx: &mut Context<Self>) -> AnyElement {
        if self.view == View::Scholar {
            return self.render_scholar_main(cx);
        }
        let papers = self.visible_papers(cx);
        let heading = match &self.view {
            View::Collection(cid) => cid.clone(),
            _ => "My Library".to_string(),
        };
        let searching = !self.search_input.read(cx).value().trim().is_empty();
        let drop_hint = match self.view {
            View::Collection(_) => format!("Drop PDFs here to add them to “{heading}”"),
            _ => "Drop PDFs here to add them to your library".to_string(),
        };

        let export_button = match &self.view {
            View::Collection(cid) => {
                let cid = cid.clone();
                Some(
                    Button::new("export-bib")
                        .outline()
                        .flex_shrink_0()
                        .icon(IconName::FileText)
                        .label("Export BibTeX")
                        .tooltip(format!(
                            "Write every paper's BibTeX, sorted by title, to Collections/{cid}/{EXPORT_FILE}"
                        ))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.export_collection_bib(&cid, window, cx)
                        })),
                )
            }
            _ => None,
        };

        let title_row = title_row(heading, count_papers(papers.len()), cx).children(export_button);
        let controls_row = controls_row(cx)
            .child(
                div().flex_1().min_w(px(120.)).child(
                    Input::new(&self.search_input)
                        .cleanable(true)
                        .prefix(Icon::new(IconName::Search).small()),
                ),
            )
            .child(
                Button::new("select-mode")
                    .flex_shrink_0()
                    .w(px(TOOLBAR_BUTTON_WIDTH))
                    .outline()
                    .icon(IconName::Check)
                    .label(if self.select_mode { "Done" } else { "Select" })
                    .selected(self.select_mode)
                    .tooltip("Select multiple papers (or Ctrl/Cmd-click, Shift-click)")
                    .on_click(cx.listener(|this, _, window, cx| this.toggle_select_mode(window, cx))),
            )
            .child(
                // Matches the Select button: same style and width.
                Button::new("import")
                    .flex_shrink_0()
                    .w(px(TOOLBAR_BUTTON_WIDTH))
                    .outline()
                    .icon(Lucide::FilePlus)
                    .label("Add")
                    .tooltip("Add PDFs…")
                    .on_click(cx.listener(Self::prompt_import)),
            );

        let body = if papers.is_empty() {
            let (title, hint) = if searching {
                ("No matching papers".to_string(), "Try a different search.".to_string())
            } else {
                ("Nothing here yet".to_string(), drop_hint)
            };
            empty_state(IconName::Inbox, title, hint, cx)
                .on_click(cx.listener(|this, _, window, cx| this.close_side_panel(window, cx)))
        } else {
            v_flex()
                .id("papers")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .children(
                    papers
                        .iter()
                        .enumerate()
                        .map(|(ix, p)| self.render_paper_row(ix, p, cx))
                        .collect::<Vec<_>>(),
                )
                // Empty space below the rows: clicking it closes the right-hand
                // pane. It's a sibling of the rows, not a parent, because
                // clicks bubble to parents and would undo the row's selection.
                .child(
                    div()
                        .id("list-background")
                        .flex_1()
                        .min_h(px(48.))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.close_side_panel(window, cx)
                        }))
                        .test_support(),
                )
        };

        v_flex()
            .flex_1()
            .min_w_0()
            .overflow_hidden()
            .h_full()
            .child(title_row)
            .child(controls_row)
            .child(
                v_flex()
                    .id("drop-area")
                    .flex_1()
                    .min_h_0()
                    .border_2()
                    .border_color(transparent_black())
                    .drag_over::<ExternalPaths>(|s, _, _, cx| {
                        s.bg(cx.theme().drop_target)
                            .border_color(cx.theme().drag_border)
                    })
                    .child(body),
            )
            .into_any_element()
    }

    /// The right-hand pane's frame: a title with a close button, then content.
    fn side_panel(&self, title: impl Into<SharedString>, cx: &Context<Self>) -> Stateful<Div> {
        let theme = cx.theme();
        v_flex()
            .id("side-panel")
            .w(px(SIDE_PANEL_WIDTH))
            .h_full()
            .flex_shrink_0()
            .overflow_y_scroll()
            .px_4()
            .pb_4()
            .border_l_1()
            .border_color(theme.border)
            .text_sm()
            .child(
                h_flex()
                    .h(px(TOOLBAR_HEIGHT))
                    .flex_shrink_0()
                    .gap_2()
                    .justify_between()
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_base()
                            .font_semibold()
                            .child(title.into()),
                    )
                    .child(
                        Button::new("close-panel")
                            .ghost()
                            .small()
                            .icon(IconName::Close)
                            .tooltip("Close (Esc)")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.close_side_panel(window, cx)
                            })),
                    ),
            )
    }

    fn render_details(&self, paper: &Paper, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let hint = theme.muted_foreground;
        let id = paper.id.clone();
        let info = paper.info().cloned().unwrap_or_default();

        let info_row = |label: &'static str, value: String| {
            h_flex()
                .gap_2()
                .text_xs()
                .child(
                    div()
                        .w(px(64.))
                        .flex_shrink_0()
                        .text_color(theme.muted_foreground)
                        .child(label),
                )
                .child(div().min_w_0().truncate().child(value))
        };

        let memberships = self.library.collections().iter().enumerate().map(|(ix, c)| {
            let cid = c.id.clone();
            let id = id.clone();
            Checkbox::new(ElementId::NamedInteger("member".into(), ix as u64))
                .label(c.name().to_string())
                .checked(c.papers.contains(&id))
                .on_click(cx.listener(move |this, checked: &bool, window, cx| {
                    this.set_membership(&cid, &id, *checked, window, cx)
                }))
        });

        let remove_from_current = match &self.view {
            View::Collection(cid) => {
                let cid = cid.clone();
                let id = id.clone();
                Some(
                    Button::new("remove-from-collection")
                        .outline()
                        .w_full()
                        .icon(Lucide::FolderMinus)
                        .label(format!("Remove from “{cid}”"))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.remove_papers_from_collection(
                                &cid,
                                &[id.clone()],
                                false,
                                window,
                                cx,
                            )
                        })),
                )
            }
            _ => None,
        };

        // PDF actions: open/replace, or attach if there's none yet.
        let pdf_actions = v_flex().mt_4().gap_2().map(|col| {
            if paper.pdf.is_some() {
                col.child(
                    Button::new("open-pdf")
                        .primary()
                        .w_full()
                        .icon(IconName::ExternalLink)
                        .label("Open PDF")
                        .on_click(cx.listener({
                            let id = id.clone();
                            move |this, _, window, cx| this.open_paper(&id, window, cx)
                        })),
                )
                .child(
                    Button::new("replace-pdf")
                        .outline()
                        .w_full()
                        .icon(Lucide::FilePlus)
                        .label("Replace PDF…")
                        .on_click(cx.listener({
                            let id = id.clone();
                            move |this, _, window, cx| this.prompt_attach_pdf(id.clone(), window, cx)
                        })),
                )
            } else {
                col.child(notice(
                    "No PDF yet",
                    "Download it (see the links below), then drop it onto this panel or choose it.",
                    cx,
                ))
                .child(
                    Button::new("attach-pdf")
                        .primary()
                        .w_full()
                        .icon(Lucide::FilePlus)
                        .label("Attach PDF…")
                        .on_click(cx.listener({
                            let id = id.clone();
                            move |this, _, window, cx| this.prompt_attach_pdf(id.clone(), window, cx)
                        })),
                )
            }
            .child(
                Button::new("reveal")
                    .outline()
                    .w_full()
                    .icon(IconName::FolderOpen)
                    .label("Show in folder")
                    .on_click(cx.listener({
                        let id = id.clone();
                        move |this, _, _, cx| {
                            if let Some(path) = this.library.any_path(&id) {
                                cx.reveal_path(&path)
                            }
                        }
                    })),
            )
            .children(link_buttons(&info, cx))
        });

        // BibTeX: show it, or offer to find it.
        let bib_section = v_flex().gap_1().map(|col| match &paper.bib {
            Some(bib) => col
                .when_some(bib.error.clone(), |col, error| {
                    col.child(notice("Can't read the .bib file", error, cx))
                })
                .when(!info.authors.is_empty(), |col| {
                    col.child(div().child(info.authors.join(", ")))
                })
                .when_some(
                    Some(
                        [info.year.clone(), info.venue.clone()]
                            .into_iter()
                            .flatten()
                            .collect::<Vec<_>>()
                            .join(" · "),
                    )
                    .filter(|s| !s.is_empty()),
                    |col, line| col.child(div().text_xs().text_color(hint).child(line)),
                )
                .when_some(info.abstract_.clone(), |col, abstract_| {
                    col.child(div().mt_1().text_xs().child(abstract_))
                })
                .child(
                    h_flex()
                        .mt_2()
                        .gap_2()
                        .child(
                            Button::new("copy-bib")
                                .outline()
                                .flex_1()
                                .icon(IconName::Copy)
                                .label("Copy BibTeX")
                                .on_click(cx.listener({
                                    let id = id.clone();
                                    move |this, _, window, cx| this.copy_bibtex(&id, window, cx)
                                })),
                        )
                        .child(
                            Button::new("edit-bib")
                                .outline()
                                .flex_1()
                                .icon(Lucide::Pencil)
                                .label("Edit .bib")
                                .tooltip("Open the .bib file in your text editor")
                                .on_click(cx.listener({
                                    let id = id.clone();
                                    move |this, _, _, cx| {
                                        if let Some(path) = this.library.bib_path(&id) {
                                            cx.open_with_system(&path)
                                        }
                                    }
                                })),
                        ),
                ),
            None => col
                .child(notice(
                    "No BibTeX yet",
                    "Find this paper on Google Scholar to save its title, authors, abstract and citation.",
                    cx,
                ))
                .child(
                    Button::new("find-on-scholar")
                        .outline()
                        .w_full()
                        .mt_1()
                        .icon(IconName::Search)
                        .label("Find on Google Scholar")
                        .on_click(cx.listener({
                            let id = id.clone();
                            move |this, _, window, cx| this.find_on_scholar(id.clone(), window, cx)
                        })),
                ),
        });

        let modified = paper
            .pdf
            .as_ref()
            .map(|f| f.modified)
            .or(paper.bib.as_ref().map(|b| b.file.modified))
            .unwrap_or(0);
        let mut info_rows = v_flex().gap_1();
        for (ix, path) in self.library.relative_paths(&id).into_iter().enumerate() {
            info_rows = info_rows.child(info_row(if ix == 0 { "Files" } else { "" }, path));
        }
        if let Some(pdf) = &paper.pdf {
            info_rows = info_rows.child(info_row("Size", format_size(pdf.size)));
        }
        info_rows = info_rows.child(info_row("Modified", format_date(modified)));

        let no_collections = self.library.collections().is_empty();
        let drop_id = id.clone();

        self.side_panel("Details", cx)
            // Drop a PDF onto the pane to attach (or replace) it.
            .drag_over::<ExternalPaths>(|s, _, _, cx| s.bg(cx.theme().drop_target))
            .on_drop(cx.listener(move |this, paths: &ExternalPaths, window, cx| {
                this.drop_pdf_on_paper(drop_id.clone(), paths.paths().to_vec(), window, cx)
            }))
            .child(section_label("NAME", hint).mt_0())
            .child(Input::new(&self.title_input))
            .child(
                div()
                    .mt_1()
                    .text_xs()
                    .text_color(hint)
                    .child("Renames the paper's files. Press Enter to save."),
            )
            .child(pdf_actions)
            .child(section_label("BIBTEX", hint))
            .child(bib_section)
            .child(section_label("INFO", hint))
            .child(info_rows)
            .child(section_label("COLLECTIONS", hint))
            .child(
                v_flex()
                    .gap_2()
                    .children(memberships)
                    .when(no_collections, |s| {
                        s.child(div().text_xs().text_color(hint).child("No collections yet."))
                    }),
            )
            .child(
                v_flex()
                    .mt_6()
                    .gap_2()
                    .children(remove_from_current)
                    .child(
                        Button::new("delete-paper")
                            .danger()
                            .w_full()
                            .icon(Lucide::Trash)
                            .label("Delete from library")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.confirm_delete_papers(vec![id.clone()], window, cx)
                            })),
                    ),
            )
            .into_any_element()
    }

    /// The right-hand pane in multi-select mode: actions for every selected paper.
    fn render_bulk_actions(&self, cx: &mut Context<Self>) -> AnyElement {
        let hint = cx.theme().muted_foreground;
        let n = self.selection.len();
        let title = if n == 0 {
            "Select papers".to_string()
        } else {
            format!("{} selected", count_papers(n))
        };

        let add_buttons = self.library.collections().iter().enumerate().map(|(ix, c)| {
            let cid = c.id.clone();
            let all_in = self.selection.iter().all(|id| c.papers.contains(id));
            Button::new(ElementId::NamedInteger("add-to".into(), ix as u64))
                .outline()
                .w_full()
                .icon(if all_in { IconName::Check } else { IconName::Folder })
                .label(c.name().to_string())
                .disabled(all_in)
                .on_click(cx.listener(move |this, _, window, cx| {
                    let ids = this.selection.clone();
                    this.add_papers_to_collection(&cid, &ids, true, window, cx)
                }))
        });

        let remove_from_current = match &self.view {
            View::Collection(cid) => {
                let cid = cid.clone();
                Some(
                    Button::new("remove-selection")
                        .outline()
                        .w_full()
                        .icon(Lucide::FolderMinus)
                        .label(format!("Remove from “{cid}”"))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            let ids = this.selection.clone();
                            this.remove_papers_from_collection(&cid, &ids, true, window, cx)
                        })),
                )
            }
            _ => None,
        };

        self.side_panel(title, cx)
            .child(
                div()
                    .text_xs()
                    .text_color(hint)
                    .child("Click papers to select them; Shift-click selects a range."),
            )
            .child(
                h_flex()
                    .mt_3()
                    .gap_2()
                    .child(
                        Button::new("select-all")
                            .outline()
                            .flex_1()
                            .label("Select all")
                            .on_click(cx.listener(|this, _, window, cx| this.select_all(window, cx))),
                    )
                    .child(
                        Button::new("clear-selection")
                            .outline()
                            .flex_1()
                            .label("Clear")
                            .disabled(n == 0)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.set_selection(Vec::new(), window, cx)
                            })),
                    ),
            )
            .when(n > 0, |panel| {
                panel
                    .child(section_label("ADD TO COLLECTION", hint))
                    .child(
                        v_flex().gap_2().children(add_buttons).child(
                            Button::new("new-collection-from-selection")
                                .ghost()
                                .w_full()
                                .icon(Lucide::FolderPlus)
                                .label("New collection…")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    let ids = this.selection.clone();
                                    this.open_collection_dialog(None, ids, window, cx)
                                })),
                        ),
                    )
                    .child(
                        v_flex()
                            .mt_6()
                            .gap_2()
                            .children(remove_from_current)
                            .child(
                                Button::new("delete-selection")
                                    .danger()
                                    .w_full()
                                    .icon(Lucide::Trash)
                                    .label(format!("Delete {}", count_papers(n)))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        let ids = this.selection.clone();
                                        this.confirm_delete_papers(ids, window, cx)
                                    })),
                            ),
                    )
            })
            .into_any_element()
    }
}

const TOOLBAR_HEIGHT: f32 = 60.;
/// Width of the Select/Done and Add buttons next to the search bar.
const TOOLBAR_BUTTON_WIDTH: f32 = 88.;
const SIDE_PANEL_WIDTH: f32 = 320.;

impl Render for PaprApp {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let side_panel = if self.view == View::Scholar {
            self.render_scholar_panel(cx)
        } else if let Some(paper) = self
            .single_selection()
            .and_then(|id| self.library.paper(id))
            .cloned()
        {
            Some(self.render_details(&paper, cx))
        } else if self.select_mode || self.selection.len() > 1 {
            Some(self.render_bulk_actions(cx))
        } else {
            None
        };

        h_flex()
            .id("papr2")
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::on_select_all))
            .on_action(cx.listener(Self::on_close_side_panel))
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            // Files dropped anywhere else in the window.
            .on_drop(cx.listener(|this, paths: &ExternalPaths, window, cx| {
                this.drop_files(paths, window, cx)
            }))
            .child(self.render_sidebar(cx))
            .child(self.render_main(cx))
            .children(side_panel)
    }
}

// ----- shared pieces ------------------------------------------------------------

/// The top row of the main panel: a large title and a muted subtitle.
fn title_row(title: impl Into<SharedString>, subtitle: impl Into<SharedString>, cx: &App) -> Div {
    h_flex()
        .gap_3()
        .px_4()
        .h(px(TOOLBAR_HEIGHT))
        .flex_shrink_0()
        .child(
            h_flex()
                .flex_1()
                .min_w_0()
                .gap_2()
                .items_baseline()
                .child(div().min_w_0().truncate().text_xl().font_semibold().child(title.into()))
                .child(
                    div()
                        .flex_shrink_0()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child(subtitle.into()),
                ),
        )
}

/// The second row of the main panel, for search and actions.
fn controls_row(cx: &App) -> Div {
    h_flex()
        .gap_2()
        .px_4()
        .pb_3()
        .flex_shrink_0()
        .border_b_1()
        .border_color(cx.theme().border)
}

fn empty_state(
    icon: IconName,
    title: impl Into<SharedString>,
    hint: impl Into<SharedString>,
    cx: &App,
) -> Stateful<Div> {
    v_flex()
        .id("empty")
        .flex_1()
        .items_center()
        .justify_center()
        .gap_2()
        .px_8()
        .text_center()
        .text_color(cx.theme().muted_foreground)
        .child(Icon::new(icon).size(px(40.)))
        .child(div().text_base().font_medium().child(title.into()))
        .child(div().text_sm().child(hint.into()))
}

/// A small inline warning, e.g. "No PDF".
fn warning_badge(text: &'static str, cx: &App) -> impl IntoElement {
    h_flex()
        .flex_shrink_0()
        .gap_1()
        .px_1p5()
        .rounded(cx.theme().radius)
        .bg(cx.theme().warning.opacity(0.15))
        .text_color(cx.theme().warning)
        .text_xs()
        .child(Icon::new(IconName::TriangleAlert).xsmall())
        .child(text)
}

/// A highlighted box explaining something that needs attention.
fn notice(title: impl Into<SharedString>, body: impl Into<SharedString>, cx: &App) -> impl IntoElement {
    v_flex()
        .gap_1()
        .p_3()
        .rounded(cx.theme().radius)
        .border_1()
        .border_color(cx.theme().warning.opacity(0.5))
        .bg(cx.theme().warning.opacity(0.08))
        .child(
            h_flex()
                .gap_1p5()
                .text_color(cx.theme().warning)
                .font_medium()
                .child(Icon::new(IconName::TriangleAlert).small())
                .child(title.into()),
        )
        .child(div().text_xs().child(body.into()))
}

/// "Open PDF link" / "Open publication page" buttons for a paper's URLs.
fn link_buttons(info: &crate::library::BibInfo, cx: &App) -> Vec<Button> {
    let _ = cx;
    let mut buttons = Vec::new();
    if let Some(url) = info.pdf_url.clone() {
        buttons.push(
            Button::new("open-pdf-link")
                .outline()
                .w_full()
                .icon(IconName::Globe)
                .label("Open PDF link")
                .tooltip(url.clone())
                .on_click(move |_, _, cx| cx.open_url(&url)),
        );
    }
    if let Some(url) = info.url.clone() {
        buttons.push(
            Button::new("open-pub-page")
                .outline()
                .w_full()
                .icon(IconName::Globe)
                .label("Open publication page")
                .tooltip(url.clone())
                .on_click(move |_, _, cx| cx.open_url(&url)),
        );
    }
    buttons
}

fn section_label(label: &'static str, color: Hsla) -> Div {
    div()
        .mt_5()
        .mb_1p5()
        .text_xs()
        .font_medium()
        .text_color(color)
        .child(label)
}

fn count_papers(n: usize) -> String {
    format!("{n} paper{}", if n == 1 { "" } else { "s" })
}

/// Collect PDFs from a path, descending into directories.
fn collect_pdfs(path: &std::path::Path, out: &mut Vec<PathBuf>) {
    if path.is_dir() {
        if let Ok(entries) = std::fs::read_dir(path) {
            let mut entries: Vec<_> = entries.flatten().map(|e| e.path()).collect();
            entries.sort();
            for entry in entries {
                collect_pdfs(&entry, out);
            }
        }
    } else if is_pdf(path) {
        out.push(path.to_path_buf());
    }
}

fn format_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    let b = bytes as f64;
    if b < KB {
        format!("{bytes} B")
    } else if b < KB * KB {
        format!("{:.0} KB", b / KB)
    } else {
        format!("{:.1} MB", b / (KB * KB))
    }
}

/// Format seconds since the Unix epoch as `YYYY-MM-DD` (UTC).
fn format_date(secs: u64) -> String {
    // Howard Hinnant's civil-from-days algorithm.
    let z = (secs / 86_400) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!("{y:04}-{m:02}-{d:02}")
}

#[cfg(test)]
mod tests {
    // Explicit imports: `gpui_kit::*` also exports GPUI's `test` macro.
    use super::{count_papers, format_date, format_size};

    #[test]
    fn formats_dates() {
        assert_eq!(format_date(0), "1970-01-01");
        assert_eq!(format_date(1_700_000_000), "2023-11-14");
    }

    #[test]
    fn formats_sizes() {
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(2048), "2 KB");
        assert_eq!(format_size(3 * 1024 * 1024), "3.0 MB");
    }

    #[test]
    fn counts_papers() {
        assert_eq!(count_papers(1), "1 paper");
        assert_eq!(count_papers(3), "3 papers");
    }
}
/// Headless UI tests: render the real view and click it with real hit-testing.
#[cfg(test)]
mod ui_tests {
    // Explicit imports: `gpui_kit::*` also exports GPUI's `test` macro.
    use super::PaprApp;
    use crate::library::Library;
    use gpui_kit::test::TestWindowExt;
    use gpui_kit::{
        AnyWindowHandle, AppContext as _, Bounds, ElementId, Entity, Point, TestAppContext,
        WindowBounds, WindowOptions, px, size,
    };
    use std::path::PathBuf;

    /// A library with PDFs "A paper", "B paper", "C paper", a BibTeX-only
    /// "D paper", and a collection "Thesis" containing "A paper".
    struct Fixture {
        root: PathBuf,
        window: AnyWindowHandle,
        app: Entity<PaprApp>,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn open(cx: &mut TestAppContext, name: &str) -> Fixture {
        let root = std::env::temp_dir().join(format!("papr2-ui-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("Library")).unwrap();
        for title in ["A paper", "B paper", "C paper"] {
            std::fs::write(root.join(format!("Library/{title}.pdf")), title).unwrap();
        }
        // "D paper" only has BibTeX (as if added from Google Scholar).
        std::fs::write(
            root.join("Library/D paper.bib"),
            "@article{d2020, title={D paper}, author={Doe, Jane}, abstract={About D.}}",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("Collections/Thesis")).unwrap();
        std::os::unix::fs::symlink("../../Library/A paper.pdf", root.join("Collections/Thesis/A paper.pdf")).unwrap();
        let library = Library::open(&root).unwrap();

        let (window, app) = cx.update(|cx| {
            gpui_kit::init(cx);
            let bounds = Bounds {
                origin: Point::default(),
                size: size(px(1200.), px(800.)),
            };
            gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    ..Default::default()
                },
                cx,
                |window, cx| cx.new(|cx| PaprApp::new(library, window, cx)),
            )
            .unwrap()
        });
        Fixture { root, window, app }
    }

    fn row(ix: u64) -> ElementId {
        ElementId::NamedInteger("paper".into(), ix)
    }

    fn click(f: &Fixture, cx: &mut TestAppContext, id: impl Into<ElementId>) {
        let id = id.into();
        cx.update_window(f.window, |_, window, cx| {
            window.render_frame(cx);
            window.click(id, cx);
        })
        .unwrap();
        cx.run_until_parked();
    }

    fn selection(f: &Fixture, cx: &mut TestAppContext) -> Vec<String> {
        f.app.read_with(cx, |app, _| app.selection.clone())
    }

    fn select_mode(f: &Fixture, cx: &mut TestAppContext) -> bool {
        f.app.read_with(cx, |app, _| app.select_mode)
    }

    fn panel_open(f: &Fixture, cx: &mut TestAppContext) -> bool {
        cx.update_window(f.window, |_, window, cx| {
            window.render_frame(cx);
            window.try_find("close-panel").is_some()
        })
        .unwrap()
    }

    #[gpui_kit::test]
    fn clicking_a_paper_selects_it_and_opens_details(cx: &mut TestAppContext) {
        let f = open(cx, "click");
        click(&f, cx, row(1));
        assert_eq!(selection(&f, cx), vec!["B paper"]);
        assert!(panel_open(&f, cx));

        // Clicking another paper switches the selection.
        click(&f, cx, row(2));
        assert_eq!(selection(&f, cx), vec!["C paper"]);
    }

    #[gpui_kit::test]
    fn select_mode_toggles_papers(cx: &mut TestAppContext) {
        let f = open(cx, "select");
        click(&f, cx, "select-mode");
        assert!(select_mode(&f, cx));

        click(&f, cx, row(0));
        click(&f, cx, row(2));
        assert_eq!(selection(&f, cx), vec!["A paper", "C paper"]);
        assert!(select_mode(&f, cx), "clicking rows must not leave select mode");

        // Clicking a selected paper deselects it.
        click(&f, cx, row(0));
        assert_eq!(selection(&f, cx), vec!["C paper"]);

        // "Done" leaves select mode and clears the selection.
        click(&f, cx, "select-mode");
        assert!(!select_mode(&f, cx));
        assert!(selection(&f, cx).is_empty());
    }

    #[gpui_kit::test]
    fn the_side_panel_closes(cx: &mut TestAppContext) {
        let f = open(cx, "close");

        // With the close button.
        click(&f, cx, row(0));
        assert!(panel_open(&f, cx));
        click(&f, cx, "close-panel");
        assert!(!panel_open(&f, cx));
        assert!(selection(&f, cx).is_empty());

        // By clicking empty space below the papers.
        click(&f, cx, row(0));
        assert!(panel_open(&f, cx));
        click(&f, cx, "list-background");
        assert!(!panel_open(&f, cx));
    }

    #[gpui_kit::test]
    fn the_open_button_does_not_toggle_selection(cx: &mut TestAppContext) {
        let f = open(cx, "open-button");
        click(&f, cx, "select-mode");
        click(&f, cx, row(0));
        assert_eq!(selection(&f, cx), vec!["A paper"]);
        // The test platform can't launch a viewer, so remove the file:
        // Open then just reports it missing.
        std::fs::remove_file(f.root.join("Library/A paper.pdf")).unwrap();
        click(&f, cx, ElementId::NamedInteger("open".into(), 0));
        assert_eq!(selection(&f, cx), vec!["A paper"]);
    }

    fn find(f: &Fixture, cx: &mut TestAppContext, id: impl Into<ElementId>) -> Option<(bool, gpui_kit::Bounds<gpui_kit::Pixels>)> {
        let id = id.into();
        cx.update_window(f.window, |_, window, cx| {
            window.render_frame(cx);
            window.try_find(id).map(|s| (s.visible(), s.bounds()))
        })
        .unwrap()
    }

    #[gpui_kit::test]
    fn papers_without_a_pdf_offer_to_attach_one(cx: &mut TestAppContext) {
        let f = open(cx, "attach");
        click(&f, cx, row(3));
        assert_eq!(selection(&f, cx), vec!["D paper"]);
        assert!(find(&f, cx, "attach-pdf").is_some());
        assert!(find(&f, cx, "open-pdf").is_none());
        assert!(find(&f, cx, "copy-bib").is_some());
        // No per-row Open button without a PDF.
        assert!(find(&f, cx, ElementId::NamedInteger("open".into(), 3)).is_none());

        // A PDF without BibTeX offers to find it instead.
        click(&f, cx, row(0));
        assert!(find(&f, cx, "find-on-scholar").is_some());
        assert!(find(&f, cx, "open-pdf").is_some());
    }

    #[gpui_kit::test]
    fn collections_export_their_bibtex(cx: &mut TestAppContext) {
        let f = open(cx, "export");
        std::fs::write(
            f.root.join("Library/A paper.bib"),
            "@article{a2020, title={A paper}}",
        )
        .unwrap();
        cx.update_window(f.window, |_, window, cx| {
            f.app.update(cx, |app, cx| app.refresh(window, cx));
        })
        .unwrap();
        assert!(find(&f, cx, "export-bib").is_none(), "only shown for collections");
        click(&f, cx, ElementId::NamedInteger("collection".into(), 0));
        click(&f, cx, "export-bib");
        let text = std::fs::read_to_string(f.root.join("Collections/Thesis/references.bib")).unwrap();
        assert!(text.contains("@article{a2020"));
    }

    #[gpui_kit::test]
    fn collection_actions_replace_the_count_on_hover(cx: &mut TestAppContext) {
        let f = open(cx, "hover");
        let count = ElementId::NamedInteger("col-count".into(), 0);
        let delete = ElementId::NamedInteger("delete-col".into(), 0);
        assert!(find(&f, cx, count.clone()).is_some_and(|(visible, _)| visible));
        assert!(!find(&f, cx, delete.clone()).is_some_and(|(visible, _)| visible));

        cx.update_window(f.window, |_, window, cx| {
            window.render_frame(cx);
            window.hover(ElementId::NamedInteger("collection".into(), 0), cx);
        })
        .unwrap();
        cx.run_until_parked();
        let (delete_visible, delete_bounds) = find(&f, cx, delete).expect("delete button");
        assert!(delete_visible);
        // The count is gone, not drawn underneath the buttons.
        if let Some((visible, bounds)) = find(&f, cx, count) {
            assert!(!visible || bounds.size.width == gpui_kit::px(0.));
        }
        let (_, row_bounds) = find(&f, cx, ElementId::NamedInteger("collection".into(), 0)).unwrap();
        assert!(delete_bounds.right() <= row_bounds.right());
    }
}
