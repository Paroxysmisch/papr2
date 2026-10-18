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

use crate::library::{CollectionId, ImportOutcome, Library, Paper, PaperId, ScanReport, is_pdf};

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
    _subscriptions: Vec<Subscription>,
    dialog_subscription: Option<Subscription>,
    _refresh_task: Task<()>,
}

impl PaprApp {
    pub fn new(library: Library, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let title_input = cx.new(|cx| InputState::new(window, cx).placeholder("Paper name"));
        let search_input = cx.new(|cx| InputState::new(window, cx).placeholder("Search papers…"));

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
            [id] => self.library.paper(id).map(|p| p.title.clone()),
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
            View::Library => self.library.papers().to_vec(),
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
            papers.retain(|p| p.title.to_lowercase().contains(&query));
        }
        papers.sort_by_key(|p| p.title.to_lowercase());
        papers
    }

    fn set_view(&mut self, view: View, window: &mut Window, cx: &mut Context<Self>) {
        // Keep only selected papers that are still visible.
        if let View::Collection(cid) = &view {
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
        let all = self.visible_papers(cx).into_iter().map(|p| p.id).collect();
        self.set_selection(all, window, cx);
    }

    /// Clears the selection and leaves multi-select mode, closing the right pane.
    fn close_side_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
        let Some(current) = self.library.paper(&id).map(|p| p.title.clone()) else {
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
            View::Library => None,
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
            self.select_mode = false;
            self.select_paper(id, window, cx);
        }
        cx.notify();
    }

    // ----- paper actions --------------------------------------------------------

    fn open_paper(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let path = self.library.paper_path(id);
        if path.exists() {
            cx.open_with_system(&path);
        } else {
            window.push_notification(
                Notification::error(format!("The file {} is missing", path.display())),
                cx,
            );
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
                let Some(paper) = self.library.paper(id) else { return };
                let n = self.library.collections_of(id).count();
                let mut description =
                    format!("{} will be deleted from disk.", self.library.relative_path(id));
                if n > 0 {
                    description.push_str(&format!(
                        " It will also be removed from {n} collection{}.",
                        if n == 1 { "" } else { "s" }
                    ));
                }
                (format!("Delete “{}”?", paper.title), description)
            }
            ids => (
                format!("Delete {}?", count_papers(ids.len())),
                "These PDFs will be deleted from Library/ and removed from every collection."
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
                (0, [id]) => Notification::info(format!(
                    "“{}” is already in “{cid}”",
                    crate::library::title_of(id)
                )),
                (0, _) => Notification::info(format!("These papers are already in “{cid}”")),
                (_, [id]) => Notification::success(format!(
                    "Added “{}” to “{cid}”",
                    crate::library::title_of(id)
                )),
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

    // ----- rendering ------------------------------------------------------------

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let library_count = self.library.papers().len();

        let library_item = h_flex()
            .id("nav-library")
            .gap_2()
            .px_2()
            .py_1p5()
            .rounded(theme.radius)
            .cursor_pointer()
            .hover(|s| s.bg(theme.sidebar_accent))
            .when(self.view == View::Library, |s| {
                s.bg(theme.sidebar_accent)
                    .text_color(theme.sidebar_accent_foreground)
                    .font_medium()
            })
            .child(Icon::new(IconName::BookOpen).small())
            .child(div().flex_1().child("My Library"))
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(library_count.to_string()),
            )
            .on_click(cx.listener(|this, _, window, cx| this.set_view(View::Library, window, cx)));

        let collections = self.library.collections().iter().enumerate().map(|(ix, c)| {
            let cid = c.id.clone();
            let active = self.view == View::Collection(cid.clone());
            let group: SharedString = format!("collection-{ix}").into();
            h_flex()
                .id(ElementId::NamedInteger("collection".into(), ix as u64))
                .group(group.clone())
                .relative()
                .gap_2()
                .pl_2()
                .pr_1()
                .py_1()
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
                .child(
                    // Overlays the paper count while hovering, so long names
                    // aren't truncated by invisible buttons.
                    h_flex()
                        .absolute()
                        .right_1()
                        .top_0()
                        .bottom_0()
                        .pl_1()
                        .rounded(theme.radius)
                        .bg(theme.sidebar_accent)
                        .invisible()
                        .group_hover(group.clone(), |s| s.visible())
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
                .child(
                    div()
                        .w(px(20.))
                        .text_right()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(c.papers.len().to_string()),
                )
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
            // Clicking anywhere in the sidebar closes the right-hand pane.
            .capture_any_mouse_down(cx.listener(|this, _: &MouseDownEvent, window, cx| {
                this.close_side_panel(window, cx)
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
            .child(v_flex().px_2().child(library_item))
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
        let collections: Vec<&str> = self.library.collections_of(&id).map(|c| c.name()).collect();
        let mut subtitle = format_size(paper.size_bytes);
        if !collections.is_empty() {
            subtitle.push_str(&format!(" · {}", collections.join(", ")));
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
                label: paper.title.clone().into(),
            }
        };

        h_flex()
            .id(ElementId::NamedInteger("paper".into(), ix as u64))
            .gap_3()
            .px_4()
            .py_2()
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
                    Checkbox::new(ElementId::NamedInteger("pick".into(), ix as u64))
                        .checked(selected),
                )
            })
            .child(Icon::new(IconName::FileText).text_color(theme.muted_foreground))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(div().truncate().font_medium().child(paper.title.clone()))
                    .child(
                        div()
                            .truncate()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(subtitle),
                    ),
            )
            .child(
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
            // Lets UI tests click rows; a no-op in normal builds.
            .test_support()
            .into_any_element()
    }

    fn render_main(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let papers = self.visible_papers(cx);
        let theme = cx.theme();
        let heading = match &self.view {
            View::Library => "My Library".to_string(),
            View::Collection(cid) => cid.clone(),
        };
        let searching = !self.search_input.read(cx).value().trim().is_empty();
        let drop_hint = match self.view {
            View::Library => "Drop PDFs here to add them to your library".to_string(),
            View::Collection(_) => format!("Drop PDFs here to add them to “{heading}”"),
        };

        // Two rows: the title (aligned with the sidebar and side-pane
        // headers), then full-size search and actions, which keeps everything
        // readable when the window is narrow.
        let title_row = h_flex()
            .px_4()
            .h(px(TOOLBAR_HEIGHT))
            .flex_shrink_0()
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_2()
                    .items_baseline()
                    .child(div().min_w_0().truncate().text_xl().font_semibold().child(heading))
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child(count_papers(papers.len())),
                    ),
            );
        let controls_row = h_flex()
            .gap_2()
            .px_4()
            .pb_3()
            .flex_shrink_0()
            .border_b_1()
            .border_color(theme.border)
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
            v_flex()
                .id("empty")
                .flex_1()
                .on_click(cx.listener(|this, _, window, cx| this.close_side_panel(window, cx)))
                .items_center()
                .justify_center()
                .gap_2()
                .text_color(theme.muted_foreground)
                .child(Icon::new(IconName::Inbox).size(px(40.)))
                .child(div().text_base().font_medium().child(title))
                .child(div().text_sm().child(hint))
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
    }

    /// The right-hand pane's frame: a title with a close button, then content.
    fn side_panel(
        &self,
        title: impl Into<SharedString>,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        let theme = cx.theme();
        v_flex()
            .id("side-panel")
            .w(px(300.))
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
        let id = paper.id.clone();

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
        let hint = theme.muted_foreground;

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
            View::Library => None,
        };

        let no_collections = self.library.collections().is_empty();

        self.side_panel("Details", cx)
            .child(section_label("NAME", hint).mt_0())
            .child(Input::new(&self.title_input))
            .child(
                div()
                    .mt_1()
                    .text_xs()
                    .text_color(hint)
                    .child("Renames the PDF file. Press Enter to save."),
            )
            .child(
                v_flex()
                    .mt_4()
                    .gap_2()
                    .child(
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
                        Button::new("reveal")
                            .outline()
                            .w_full()
                            .icon(IconName::FolderOpen)
                            .label("Show in folder")
                            .on_click(cx.listener({
                                let id = id.clone();
                                move |this, _, _, cx| {
                                    cx.reveal_path(&this.library.paper_path(&id))
                                }
                            })),
                    ),
            )
            .child(section_label("INFO", hint))
            .child(
                v_flex()
                    .gap_1()
                    .child(info_row("File", self.library.relative_path(&id)))
                    .child(info_row("Size", format_size(paper.size_bytes)))
                    .child(info_row("Modified", format_date(paper.modified))),
            )
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
            View::Library => None,
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

impl Render for PaprApp {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let side_panel = if let Some(paper) = self
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
            // Files dropped anywhere in the window go into the current view.
            .on_drop(cx.listener(|this, paths: &ExternalPaths, window, cx| {
                this.import_paths(paths.paths(), None, window, cx)
            }))
            .child(self.render_sidebar(cx))
            .child(self.render_main(cx))
            .children(side_panel)
    }
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

    /// A library with three papers: "A paper", "B paper", "C paper".
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
        assert_eq!(selection(&f, cx), vec!["B paper.pdf"]);
        assert!(panel_open(&f, cx));

        // Clicking another paper switches the selection.
        click(&f, cx, row(2));
        assert_eq!(selection(&f, cx), vec!["C paper.pdf"]);
    }

    #[gpui_kit::test]
    fn select_mode_toggles_papers(cx: &mut TestAppContext) {
        let f = open(cx, "select");
        click(&f, cx, "select-mode");
        assert!(select_mode(&f, cx));

        click(&f, cx, row(0));
        click(&f, cx, row(2));
        assert_eq!(selection(&f, cx), vec!["A paper.pdf", "C paper.pdf"]);
        assert!(select_mode(&f, cx), "clicking rows must not leave select mode");

        // Clicking a selected paper deselects it.
        click(&f, cx, row(0));
        assert_eq!(selection(&f, cx), vec!["C paper.pdf"]);

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
        assert_eq!(selection(&f, cx), vec!["A paper.pdf"]);
        // The test platform can't launch a viewer, so remove the file:
        // Open then just reports it missing.
        std::fs::remove_file(f.root.join("Library/A paper.pdf")).unwrap();
        click(&f, cx, ElementId::NamedInteger("open".into(), 0));
        assert_eq!(selection(&f, cx), vec!["A paper.pdf"]);
    }
}
