use std::{cell::Cell, path::PathBuf, time::Duration};

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::StyledExt as _;
use gpui_kit::component::{
    ActiveTheme, Icon, IconName, Sizable, WindowExt,
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

/// Which list is shown in the main panel.
#[derive(Clone, PartialEq, Eq)]
enum View {
    Library,
    Collection(CollectionId),
}

/// Payload for dragging a paper row onto a collection in the sidebar.
#[derive(Clone)]
struct DraggedPaper {
    id: PaperId,
    title: SharedString,
}

impl Render for DraggedPaper {
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
            .child(div().truncate().child(self.title.clone()))
    }
}

pub struct PaprApp {
    library: Library,
    view: View,
    selected: Option<PaperId>,
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

        let subscriptions = vec![
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

        Self {
            library,
            view: View::Library,
            selected: None,
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

        if let Some(sel) = self.selected.clone() {
            let renamed_to = report
                .renamed_papers
                .iter()
                .find(|(old, _)| *old == sel)
                .map(|(_, new)| new.clone());
            if let Some(new) = renamed_to {
                self.selected = Some(new);
                self.sync_title_input(false, window, cx);
            } else if self.library.paper(&sel).is_none() {
                self.selected = None;
                self.sync_title_input(true, window, cx);
            }
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
        let title = self
            .selected
            .as_deref()
            .and_then(|id| self.library.paper(id))
            .map(|p| p.title.clone())
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
        // Keep the selection only if the paper is still visible.
        if let (Some(sel), View::Collection(cid)) = (&self.selected, &view) {
            let visible = self
                .library
                .collection(cid)
                .is_some_and(|c| c.papers.contains(sel));
            if !visible {
                self.select_paper(None, window, cx);
            }
        }
        self.view = view;
        cx.notify();
    }

    fn select_paper(&mut self, id: Option<PaperId>, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected == id {
            return;
        }
        // Don't lose an in-progress rename when switching papers.
        self.commit_title(window, cx);
        self.selected = id;
        self.sync_title_input(true, window, cx);
        cx.notify();
    }

    fn commit_title(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.selected.clone() else { return };
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
            Ok(new_id) => self.selected = Some(new_id),
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

        let mut msg = format!("Imported {added} paper{}", if added == 1 { "" } else { "s" });
        if duplicates > 0 {
            msg.push_str(&format!(" ({duplicates} already in library)"));
        }
        if let Some(cid) = &target {
            msg.push_str(&format!(" into “{cid}”"));
        }
        window.push_notification(Notification::success(msg), cx);

        if last.is_some() {
            self.select_paper(last, window, cx);
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

    fn confirm_delete_paper(&mut self, id: PaperId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(paper) = self.library.paper(&id) else { return };
        let title = paper.title.clone();
        let path = self.library.relative_path(&id);
        let n = self.library.collections_of(&id).count();
        let this = cx.entity().downgrade();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let this = this.clone();
            let id = id.clone();
            let mut description = format!("{path} will be deleted from disk.");
            if n > 0 {
                description.push_str(&format!(
                    " It will also be removed from {n} collection{}.",
                    if n == 1 { "" } else { "s" }
                ));
            }
            alert
                .title(format!("Delete “{title}”?"))
                .description(description)
                .show_cancel(true)
                .ok_text("Delete")
                .ok_variant(ButtonVariant::Danger)
                .on_ok(move |_, window, cx| {
                    let id = id.clone();
                    this.update(cx, |this, cx| {
                        if this.selected.as_ref() == Some(&id) {
                            this.select_paper(None, window, cx);
                        }
                        if let Err(err) = this.library.delete_paper(&id) {
                            Self::notify_error(window, cx, err);
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
        let result = if member {
            self.library.add_to_collection(cid, pid).map(|_| ())
        } else {
            self.library.remove_from_collection(cid, pid)
        };
        if let Err(err) = result {
            Self::notify_error(window, cx, err);
        }
        if !member
            && self.view == View::Collection(cid.to_string())
            && self.selected.as_deref() == Some(pid)
        {
            self.select_paper(None, window, cx);
        }
        cx.notify();
    }

    fn drop_paper_on_collection(
        &mut self,
        cid: &str,
        paper: &DraggedPaper,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.library.add_to_collection(cid, &paper.id) {
            Ok(true) => window.push_notification(
                Notification::success(format!("Added “{}” to “{cid}”", paper.title)),
                cx,
            ),
            Ok(false) => window.push_notification(
                Notification::info(format!("“{}” is already in “{cid}”", paper.title)),
                cx,
            ),
            Err(err) => Self::notify_error(window, cx, err),
        }
        cx.notify();
    }

    // ----- collection actions -----------------------------------------------------

    /// Opens a dialog to create a collection (`existing == None`) or rename one.
    fn open_collection_dialog(
        &mut self,
        existing: Option<CollectionId>,
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
        let existing_enter = existing.clone();
        self.dialog_subscription = Some(cx.subscribe_in(
            &input,
            window,
            move |this, input, ev: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = ev {
                    let name = input.read(cx).value().to_string();
                    if this.submit_collection_dialog(existing_enter.clone(), &name, window, cx) {
                        window.close_dialog(cx);
                    }
                }
            },
        ));

        let this = cx.entity().downgrade();
        let first_build = Cell::new(true);
        let (title, ok) = match existing {
            Some(_) => ("Rename collection", "Rename"),
            None => ("New collection", "Create"),
        };
        window.open_dialog(cx, move |dialog, window, cx| {
            if first_build.replace(false) {
                input.focus_handle(cx).focus(window, cx);
            }
            let input_ok = input.clone();
            let this = this.clone();
            let existing = existing.clone();
            dialog
                .title(title)
                .w(px(420.))
                .child(Input::new(&input))
                .footer(
                    DialogFooter::new()
                        .child(DialogClose::new().trigger(|b| b.outline().label("Cancel")))
                        .child(DialogAction::new().child(Button::new("ok").primary().label(ok))),
                )
                .on_ok(move |_, window, cx| {
                    let name = input_ok.read(cx).value().to_string();
                    this.update(cx, |this, cx| {
                        this.submit_collection_dialog(existing.clone(), &name, window, cx)
                    })
                    .unwrap_or(true)
                })
        });
    }

    /// Returns `true` if the dialog should close.
    fn submit_collection_dialog(
        &mut self,
        existing: Option<CollectionId>,
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
                self.view = View::Collection(new_id);
                self.selected = None;
                self.sync_title_input(true, window, cx);
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
                .drag_over::<DraggedPaper>(|s, _, _, cx| {
                    s.bg(cx.theme().drop_target)
                        .border_color(cx.theme().drag_border)
                })
                .drag_over::<ExternalPaths>(|s, _, _, cx| {
                    s.bg(cx.theme().drop_target)
                        .border_color(cx.theme().drag_border)
                })
                .on_drop(cx.listener({
                    let cid = cid.clone();
                    move |this, paper: &DraggedPaper, window, cx| {
                        this.drop_paper_on_collection(&cid, paper, window, cx)
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
                                        this.open_collection_dialog(Some(cid.clone()), window, cx)
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
            .w(px(230.))
            .h_full()
            .flex_shrink_0()
            .bg(theme.sidebar)
            .text_color(theme.sidebar_foreground)
            .border_r_1()
            .border_color(theme.sidebar_border)
            .text_sm()
            .child(
                h_flex()
                    .gap_2()
                    .pl_4()
                    .pr_2()
                    .h(px(52.))
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
                                this.open_collection_dialog(None, window, cx)
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
        let selected = self.selected.as_ref() == Some(&id);
        let collections: Vec<&str> = self.library.collections_of(&id).map(|c| c.name()).collect();
        let mut subtitle = format_size(paper.size_bytes);
        if !collections.is_empty() {
            subtitle.push_str(&format!(" · {}", collections.join(", ")));
        }
        let drag = DraggedPaper {
            id: id.clone(),
            title: paper.title.clone().into(),
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
                move |this, ev: &ClickEvent, window, cx| {
                    this.select_paper(Some(id.clone()), window, cx);
                    if let ClickEvent::Mouse(m) = ev {
                        if m.down.click_count == 2 {
                            this.open_paper(&id, window, cx);
                        }
                    }
                }
            }))
            .on_drag(drag, |drag, _, _, cx| cx.new(|_| drag.clone()))
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
                        this.open_paper(&id, window, cx)
                    })),
            )
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

        let toolbar = h_flex()
            .gap_3()
            .px_4()
            .h(px(52.))
            .flex_shrink_0()
            .border_b_1()
            .border_color(theme.border)
            .child(
                h_flex()
                    .flex_1()
                    .min_w(px(140.))
                    .gap_2()
                    .items_baseline()
                    .child(div().truncate().text_lg().font_semibold().child(heading))
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child(format!(
                                "{} paper{}",
                                papers.len(),
                                if papers.len() == 1 { "" } else { "s" }
                            )),
                    ),
            )
            .child(
                div().w(px(240.)).min_w(px(100.)).flex_shrink(1.).child(
                    Input::new(&self.search_input)
                        .small()
                        .cleanable(true)
                        .prefix(Icon::new(IconName::Search).small()),
                ),
            )
            .child(
                Button::new("import")
                    .primary()
                    .small()
                    .icon(Lucide::FilePlus)
                    .label("Add PDFs…")
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
        };

        v_flex()
            .flex_1()
            .min_w_0()
            .overflow_hidden()
            .h_full()
            .child(toolbar)
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

    fn render_details(&self, paper: &Paper, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let id = paper.id.clone();

        let section = |label: &'static str| {
            div()
                .mt_4()
                .mb_1()
                .text_xs()
                .font_medium()
                .text_color(theme.muted_foreground)
                .child(label)
        };
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
                        .small()
                        .w_full()
                        .icon(Lucide::FolderMinus)
                        .label(format!("Remove from “{cid}”"))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.set_membership(&cid, &id, false, window, cx)
                        })),
                )
            }
            View::Library => None,
        };

        v_flex()
            .id("details")
            .w(px(290.))
            .h_full()
            .flex_shrink_0()
            .overflow_y_scroll()
            .p_4()
            .border_l_1()
            .border_color(theme.border)
            .text_sm()
            .child(section("NAME"))
            .child(Input::new(&self.title_input))
            .child(
                div()
                    .mt_1()
                    .text_xs()
                    .text_color(theme.muted_foreground)
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
                            .small()
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
            .child(section("INFO"))
            .child(
                v_flex()
                    .gap_1()
                    .child(info_row("File", self.library.relative_path(&id)))
                    .child(info_row("Size", format_size(paper.size_bytes)))
                    .child(info_row("Modified", format_date(paper.modified))),
            )
            .child(section("COLLECTIONS"))
            .child(
                v_flex()
                    .gap_2()
                    .children(memberships)
                    .when(self.library.collections().is_empty(), |s| {
                        s.child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child("No collections yet."),
                        )
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
                            .small()
                            .w_full()
                            .icon(Lucide::Trash)
                            .label("Delete from library")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.confirm_delete_paper(id.clone(), window, cx)
                            })),
                    ),
            )
            .into_any_element()
    }
}

impl Render for PaprApp {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let selected = self
            .selected
            .as_deref()
            .and_then(|id| self.library.paper(id))
            .cloned();

        h_flex()
            .id("papr2")
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            // Files dropped anywhere in the window go into the current view.
            .on_drop(cx.listener(|this, paths: &ExternalPaths, window, cx| {
                this.import_paths(paths.paths(), None, window, cx)
            }))
            .child(self.render_sidebar(cx))
            .child(self.render_main(cx))
            .children(selected.map(|paper| self.render_details(&paper, cx)))
    }
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
    use super::{format_date, format_size};

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
}
