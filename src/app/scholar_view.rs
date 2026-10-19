//! The Google Scholar view: search, pick a result, review/edit its BibTeX,
//! save it to the library, then attach the PDF you downloaded.

use std::collections::BTreeSet;

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::{
    ActiveTheme, Disableable, Icon, IconName, Sizable, StyledExt as _, WindowExt,
    button::{Button, ButtonVariants},
    checkbox::Checkbox,
    h_flex,
    input::{Input, InputEvent, InputState, Textarea, TextareaState},
    notification::Notification,
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::{
    PaprApp, TOOLBAR_BUTTON_WIDTH, View, controls_row, empty_state, link_buttons, notice,
    section_label, title_row,
};
use crate::bibtex::{self, Entry};
use crate::library::{BibInfo, CollectionId, Library, PaperId};
use crate::scholar::{self, SearchResult};

pub(super) enum SearchStatus {
    Idle,
    Searching,
    Failed(String),
    Done,
}

pub(super) struct ScholarState {
    query: Entity<InputState>,
    status: SearchStatus,
    results: Vec<SearchResult>,
    /// The result shown in the side pane.
    selected: Option<usize>,
    /// Fetching the selected result's BibTeX.
    loading: bool,
    draft: Option<Draft>,
    /// Looking up metadata for this existing paper (one with a PDF but no
    /// .bib): the saved entry becomes its .bib, keeping its name.
    attach_to: Option<PaperId>,
    search_task: Option<Task<()>>,
    bibtex_task: Option<Task<()>>,
}

/// An editable BibTeX entry for the selected result.
pub(super) struct Draft {
    result: usize,
    /// Why Google Scholar's BibTeX couldn't be used, if it couldn't.
    fetch_error: Option<String>,
    kind: Entity<InputState>,
    key: Entity<InputState>,
    fields: Vec<DraftField>,
    /// Collections to add the paper to when saving.
    collections: BTreeSet<CollectionId>,
    /// Set once saved: the new paper.
    saved: Option<PaperId>,
}

struct DraftField {
    name: String,
    input: FieldInput,
}

enum FieldInput {
    Line(Entity<InputState>),
    Text(Entity<TextareaState>),
}

impl FieldInput {
    fn value(&self, cx: &App) -> String {
        match self {
            FieldInput::Line(input) => input.read(cx).value().to_string(),
            FieldInput::Text(input) => input.read(cx).value().to_string(),
        }
    }
}

/// Fields shown first, and fields always offered (even if empty).
const LEADING_FIELDS: [&str; 3] = ["title", "author", "year"];
const TRAILING_FIELDS: [&str; 3] = ["abstract", "url", "pdf"];

impl ScholarState {
    pub(super) fn new(window: &mut Window, cx: &mut Context<PaprApp>) -> (Self, Subscription) {
        let query = cx.new(|cx| {
            InputState::new(window, cx).placeholder("Title, authors or keywords…")
        });
        let subscription = cx.subscribe_in(&query, window, |this, _, ev: &InputEvent, window, cx| {
            if let InputEvent::PressEnter { .. } = ev {
                this.scholar_search(window, cx);
            }
        });
        let state = Self {
            query,
            status: SearchStatus::Idle,
            results: Vec::new(),
            selected: None,
            loading: false,
            draft: None,
            attach_to: None,
            search_task: None,
            bibtex_task: None,
        };
        (state, subscription)
    }

    pub(super) fn close_result(&mut self) {
        self.selected = None;
        self.loading = false;
        self.draft = None;
        self.bibtex_task = None;
    }

    /// Keep pointing at papers renamed in the file manager.
    pub(super) fn follow_renames(&mut self, renames: &[(PaperId, PaperId)]) {
        let follow = |id: &mut PaperId| {
            if let Some((_, new)) = renames.iter().find(|(old, _)| old == id) {
                *id = new.clone();
            }
        };
        if let Some(id) = self.attach_to.as_mut() {
            follow(id);
        }
        if let Some(id) = self.draft.as_mut().and_then(|d| d.saved.as_mut()) {
            follow(id);
        }
    }

    /// The paper just saved from Scholar, if it still needs its PDF.
    pub(super) fn paper_awaiting_pdf(&self, library: &Library) -> Option<PaperId> {
        let id = self.draft.as_ref()?.saved.clone()?;
        library
            .paper(&id)
            .is_some_and(|p| p.pdf.is_none())
            .then_some(id)
    }
}

impl PaprApp {
    // ----- actions ----------------------------------------------------------

    pub(super) fn scholar_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let query = self.scholar.query.read(cx).value().trim().to_string();
        if query.is_empty() {
            return;
        }
        self.scholar.close_result();
        self.scholar.results.clear();
        self.scholar.status = SearchStatus::Searching;
        self.scholar.search_task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { scholar::search(&query) })
                .await;
            this.update_in(cx, |this, _, cx| {
                match result {
                    Ok(results) => {
                        this.scholar.results = results;
                        this.scholar.status = SearchStatus::Done;
                    }
                    Err(err) => this.scholar.status = SearchStatus::Failed(format!("{err:#}")),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// Shows a result in the side pane and fetches its BibTeX.
    fn scholar_select(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.scholar.selected == Some(ix) {
            return;
        }
        let Some(result) = self.scholar.results.get(ix).cloned() else {
            return;
        };
        self.scholar.close_result();
        self.scholar.selected = Some(ix);
        self.scholar.loading = true;
        self.scholar.bibtex_task = Some(cx.spawn_in(window, async move |this, cx| {
            let fetched = cx
                .background_executor()
                .spawn({
                    let result = result.clone();
                    async move { scholar::fetch_bibtex(&result) }
                })
                .await
                .map_err(|e| format!("{e:#}"));
            this.update_in(cx, |this, window, cx| {
                if this.scholar.selected == Some(ix) {
                    this.scholar.loading = false;
                    this.scholar.draft = Some(build_draft(ix, &result, fetched, window, cx));
                    cx.notify();
                }
            })
            .ok();
        }));
        cx.notify();
    }

    /// Looks up BibTeX for an existing paper that has none.
    pub(super) fn find_on_scholar(&mut self, id: PaperId, window: &mut Window, cx: &mut Context<Self>) {
        self.set_view(View::Scholar, window, cx);
        self.scholar.query.update(cx, |q, cx| q.set_value(id.clone(), window, cx));
        self.scholar.attach_to = Some(id);
        self.scholar_search(window, cx);
    }

    fn show_in_library(&mut self, id: PaperId, window: &mut Window, cx: &mut Context<Self>) {
        self.set_view(View::Library, window, cx);
        self.select_paper(id, window, cx);
    }

    /// Checks the draft and works out where it would be saved.
    /// Returns the paper name, the entry, and a note for the user.
    fn check_draft(&self, cx: &App) -> Result<(PaperId, Entry, Option<String>), String> {
        let draft = self.scholar.draft.as_ref().ok_or("Nothing to save")?;
        let fields: Vec<(String, String)> = draft
            .fields
            .iter()
            .map(|f| (f.name.clone(), f.input.value(cx).trim().to_string()))
            .filter(|(_, value)| !value.is_empty())
            .collect();
        let entry = Entry {
            kind: draft.kind.read(cx).value().trim().to_lowercase(),
            key: draft.key.read(cx).value().trim().to_string(),
            fields,
        };
        let Some(title) = entry.get("title") else {
            return Err("A title is needed; it also becomes the file name.".into());
        };
        entry.validate()?;

        match &self.scholar.attach_to {
            Some(id) => {
                if self.library.has_bib(id) {
                    return Err(format!("“{id}” already has a BibTeX file."));
                }
                let note = format!("Saves Library/{id}.bib, next to the existing PDF.");
                Ok((id.clone(), entry, Some(note)))
            }
            None => {
                let stem = Library::stem_for_title(title);
                if self.library.has_bib(&stem) {
                    return Err(format!(
                        "Your library already has “{stem}.bib”. Change the title to save it under another name."
                    ));
                }
                let note = if self.library.paper(&stem).is_some() {
                    format!("Saves Library/{stem}.bib and pairs it with the existing {stem}.pdf.")
                } else {
                    format!("Saves Library/{stem}.bib")
                };
                Ok((stem, entry, Some(note)))
            }
        }
    }

    fn scholar_save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (stem, entry, _) = match self.check_draft(cx) {
            Ok(checked) => checked,
            Err(err) => {
                window.push_notification(Notification::error(err), cx);
                return;
            }
        };
        match self.library.save_bib(&stem, &entry) {
            Ok(id) => {
                let collections = self
                    .scholar
                    .draft
                    .as_ref()
                    .map(|d| d.collections.iter().cloned().collect::<Vec<_>>())
                    .unwrap_or_default();
                for cid in collections {
                    self.add_papers_to_collection(&cid, &[id.clone()], false, window, cx);
                }
                let has_pdf = self.library.paper(&id).is_some_and(|p| p.pdf.is_some());
                window.push_notification(
                    Notification::success(if has_pdf {
                        format!("Saved the BibTeX for “{id}”")
                    } else {
                        format!("Added “{id}” to your library. Now attach its PDF.")
                    }),
                    cx,
                );
                self.scholar.attach_to = None;
                if let Some(draft) = self.scholar.draft.as_mut() {
                    draft.saved = Some(id);
                }
            }
            Err(err) => Self::notify_error(window, cx, err),
        }
        cx.notify();
    }

    // ----- rendering ----------------------------------------------------------

    pub(super) fn render_scholar_main(&self, cx: &mut Context<Self>) -> AnyElement {
        let s = &self.scholar;
        let searching = matches!(s.status, SearchStatus::Searching);
        let subtitle = match s.status {
            SearchStatus::Searching => "Searching…".to_string(),
            SearchStatus::Done => format!(
                "{} result{}",
                s.results.len(),
                if s.results.len() == 1 { "" } else { "s" }
            ),
            _ => String::new(),
        };

        let banner = s.attach_to.clone().map(|id| {
            h_flex()
                .gap_2()
                .mx_4()
                .mb_3()
                .px_3()
                .py_2()
                .rounded(cx.theme().radius)
                .bg(cx.theme().accent)
                .text_sm()
                .child(Icon::new(IconName::Info).small())
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(format!("Finding BibTeX for “{id}”. The entry you save becomes {id}.bib.")),
                )
                .child(
                    Button::new("cancel-attach")
                        .ghost()
                        .small()
                        .label("Cancel")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.scholar.attach_to = None;
                            cx.notify();
                        })),
                )
        });

        let controls = controls_row(cx)
            .child(
                div().flex_1().min_w(px(120.)).child(
                    Input::new(&s.query)
                        .cleanable(true)
                        .prefix(Icon::new(IconName::Search).small()),
                ),
            )
            .child(
                Button::new("scholar-search")
                    .flex_shrink_0()
                    .w(px(TOOLBAR_BUTTON_WIDTH))
                    .outline()
                    .label("Search")
                    .loading(searching)
                    .disabled(searching)
                    .on_click(cx.listener(|this, _, window, cx| this.scholar_search(window, cx))),
            );

        let body = match &s.status {
            SearchStatus::Idle => empty_state(
                IconName::Search,
                "Search Google Scholar",
                "Pick a result to save its BibTeX to your library, then attach the PDF you download.",
                cx,
            )
            .into_any_element(),
            SearchStatus::Searching => empty_state(
                IconName::LoaderCircle,
                "Searching Google Scholar…",
                "The first search also sets up papr2's Python environment, which can take a minute.",
                cx,
            )
            .into_any_element(),
            SearchStatus::Failed(err) => {
                empty_state(IconName::TriangleAlert, "The search failed", err.clone(), cx)
                    .into_any_element()
            }
            SearchStatus::Done if s.results.is_empty() => {
                empty_state(IconName::Inbox, "No results", "Try different keywords.", cx)
                    .into_any_element()
            }
            SearchStatus::Done => v_flex()
                .id("scholar-results")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .children(
                    s.results
                        .iter()
                        .enumerate()
                        .map(|(ix, r)| self.render_result_row(ix, r, cx))
                        .collect::<Vec<_>>(),
                )
                .into_any_element(),
        };

        v_flex()
            .flex_1()
            .min_w_0()
            .overflow_hidden()
            .h_full()
            .child(title_row("Google Scholar", subtitle, cx))
            .children(banner)
            .child(controls)
            .child(body)
            .into_any_element()
    }

    fn render_result_row(&self, ix: usize, r: &SearchResult, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let selected = self.scholar.selected == Some(ix);
        let in_library = self.library.has_bib(&Library::stem_for_title(&r.title));
        let byline = [
            (!r.authors.is_empty()).then(|| r.authors.join(", ")),
            (!r.year.is_empty()).then(|| r.year.clone()),
            (!r.venue.is_empty()).then(|| r.venue.clone()),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" · ");

        v_flex()
            .id(ElementId::NamedInteger("result".into(), ix as u64))
            .gap_0p5()
            .px_4()
            .py_2p5()
            .border_b_1()
            .border_color(theme.border)
            .cursor_pointer()
            .hover(|s| s.bg(theme.list_hover))
            .when(selected, |s| s.bg(theme.list_active))
            .on_click(cx.listener(move |this, _, window, cx| this.scholar_select(ix, window, cx)))
            .child(
                h_flex()
                    .gap_2()
                    .child(div().min_w_0().truncate().font_medium().child(r.title.clone()))
                    .when(in_library, |row| {
                        row.child(
                            div()
                                .flex_shrink_0()
                                .px_1p5()
                                .rounded(theme.radius)
                                .bg(theme.success.opacity(0.15))
                                .text_color(theme.success)
                                .text_xs()
                                .child("In library"),
                        )
                    }),
            )
            .when(!byline.is_empty(), |col| {
                col.child(div().truncate().text_sm().child(byline))
            })
            .when(!r.abstract_.is_empty(), |col| {
                col.child(
                    div()
                        .text_xs()
                        // Without an ellipsis, gpui puts all remaining text on the last
                        // line and clips it mid-word.
                        .line_clamp(2)
                        .text_ellipsis()
                        .text_color(theme.muted_foreground)
                        .child(r.abstract_.clone()),
                )
            })
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(format!(
                        "Cited by {}{}",
                        r.num_citations,
                        if r.eprint_url.is_empty() { "" } else { " · PDF link available" }
                    )),
            )
            .test_support()
            .into_any_element()
    }

    pub(super) fn render_scholar_panel(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let ix = self.scholar.selected?;
        let result = self.scholar.results.get(ix)?;
        let links = BibInfo {
            url: Some(result.pub_url.clone()).filter(|u| !u.is_empty()),
            pdf_url: Some(result.eprint_url.clone()).filter(|u| !u.is_empty()),
            ..Default::default()
        };
        let draft = self.scholar.draft.as_ref().filter(|d| d.result == ix);

        let panel = match draft {
            Some(draft) if draft.saved.is_some() => self.render_saved(draft, &links, cx),
            Some(draft) => self.render_editor(draft, result, &links, cx),
            None => self
                .side_panel("Add to library", cx)
                .child(div().font_medium().child(result.title.clone()))
                .child(
                    h_flex()
                        .mt_3()
                        .gap_2()
                        .text_color(cx.theme().muted_foreground)
                        .child(Icon::new(IconName::LoaderCircle).small())
                        .child("Fetching the BibTeX from Google Scholar…"),
                )
                .child(v_flex().mt_4().gap_2().children(link_buttons(&links, cx)))
                .into_any_element(),
        };
        Some(panel)
    }

    fn render_editor(
        &self,
        draft: &Draft,
        result: &SearchResult,
        links: &BibInfo,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let hint = cx.theme().muted_foreground;
        let check = self.check_draft(cx);

        let field_label = |name: &str| {
            let label = match name {
                "title" if self.scholar.attach_to.is_none() => "title (also the file name)".to_string(),
                "pdf" => "pdf (link to the PDF)".to_string(),
                "url" => "url (publication page)".to_string(),
                _ => name.to_string(),
            };
            div().mt_2().mb_1().text_xs().text_color(hint).child(label)
        };

        let fields = draft.fields.iter().map(|f| {
            let input = match &f.input {
                FieldInput::Line(state) => Input::new(state).into_any_element(),
                FieldInput::Text(state) => Textarea::new(state).into_any_element(),
            };
            v_flex().child(field_label(&f.name)).child(input)
        });

        let collections = self.library.collections().iter().enumerate().map(|(ix, c)| {
            let cid = c.id.clone();
            Checkbox::new(ElementId::NamedInteger("draft-col".into(), ix as u64))
                .label(c.name().to_string())
                .checked(draft.collections.contains(&cid))
                .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                    if let Some(draft) = this.scholar.draft.as_mut() {
                        if *checked {
                            draft.collections.insert(cid.clone());
                        } else {
                            draft.collections.remove(&cid);
                        }
                    }
                    cx.notify();
                }))
        });

        let (status, can_save) = match &check {
            Ok((_, _, note)) => (
                div().text_xs().text_color(hint).children(note.clone()),
                true,
            ),
            Err(err) => (div().text_xs().text_color(cx.theme().danger).child(err.clone()), false),
        };

        self.side_panel("Add to library", cx)
            .child(div().font_medium().child(result.title.clone()))
            .when(!links.url.is_none() || !links.pdf_url.is_none(), |panel| {
                panel.child(v_flex().mt_3().gap_2().children(link_buttons(links, cx)))
            })
            .when_some(draft.fetch_error.clone(), |panel, err| {
                panel.child(div().mt_3().child(notice(
                    "Couldn't get Google Scholar's BibTeX",
                    format!("The fields below were filled from the search result instead. ({err})"),
                    cx,
                )))
            })
            .child(section_label("BIBTEX", hint))
            .child(
                div()
                    .text_xs()
                    .text_color(hint)
                    .child("Edit anything before saving. Empty fields are left out."),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(v_flex().w(px(110.)).child(field_label("type")).child(Input::new(&draft.kind)))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(field_label("citation key"))
                            .child(Input::new(&draft.key)),
                    ),
            )
            .children(fields)
            .when(!self.library.collections().is_empty(), |panel| {
                panel
                    .child(section_label("ALSO ADD TO", hint))
                    .child(v_flex().gap_2().children(collections))
            })
            .child(
                v_flex()
                    .mt_5()
                    .gap_2()
                    .child(status)
                    .child(
                        Button::new("save-bib")
                            .primary()
                            .w_full()
                            .icon(IconName::Check)
                            .label("Save to library")
                            .disabled(!can_save)
                            .on_click(cx.listener(|this, _, window, cx| this.scholar_save(window, cx))),
                    ),
            )
            .into_any_element()
    }

    fn render_saved(&self, draft: &Draft, links: &BibInfo, cx: &mut Context<Self>) -> AnyElement {
        let hint = cx.theme().muted_foreground;
        let id = draft.saved.clone().unwrap_or_default();
        let has_pdf = self.library.paper(&id).is_some_and(|p| p.pdf.is_some());
        let still_there = self.library.paper(&id).is_some();

        let panel = self
            .side_panel("Add to library", cx)
            .child(
                h_flex()
                    .gap_2()
                    .text_color(cx.theme().success)
                    .font_medium()
                    .child(Icon::new(IconName::CircleCheck).small())
                    .child(format!("Saved Library/{id}.bib")),
            )
            .child(div().mt_1().child(id.clone()));

        if !still_there {
            return panel
                .child(div().mt_3().text_xs().text_color(hint).child("This paper is no longer in your library."))
                .into_any_element();
        }

        let open = cx.listener({
            let id = id.clone();
            move |this, _, window, cx| this.show_in_library(id.clone(), window, cx)
        });
        let panel = if has_pdf {
            panel.child(
                h_flex()
                    .mt_4()
                    .gap_2()
                    .text_color(cx.theme().success)
                    .child(Icon::new(IconName::CircleCheck).small())
                    .child("PDF attached"),
            )
        } else {
            let drop_id = id.clone();
            let choose_id = id.clone();
            panel
                .child(section_label("ATTACH THE PDF", hint))
                .child(div().text_xs().child("1. Download the PDF:"))
                .child(v_flex().mt_2().gap_2().children(link_buttons(links, cx)))
                .when(links.url.is_none() && links.pdf_url.is_none(), |p| {
                    p.child(
                        div()
                            .text_xs()
                            .text_color(hint)
                            .child("Google Scholar gave no link for this result; search for it on the web."),
                    )
                })
                .child(div().mt_3().text_xs().child("2. Drop it here, or choose it:"))
                .child(
                    v_flex()
                        .id("pdf-drop-zone")
                        .mt_2()
                        .h(px(96.))
                        .items_center()
                        .justify_center()
                        .gap_1()
                        .rounded(cx.theme().radius)
                        .border_2()
                        .border_dashed()
                        .border_color(cx.theme().border)
                        .text_color(hint)
                        .drag_over::<ExternalPaths>(|s, _, _, cx| {
                            s.bg(cx.theme().drop_target).border_color(cx.theme().drag_border)
                        })
                        .on_drop(cx.listener(move |this, paths: &ExternalPaths, window, cx| {
                            this.attach_pdf_from(&drop_id, paths.paths(), window, cx)
                        }))
                        .child(Icon::new(Lucide::FilePlus))
                        .child(div().text_xs().child("Drop the PDF here")),
                )
                .child(
                    Button::new("choose-pdf")
                        .outline()
                        .w_full()
                        .mt_2()
                        .icon(IconName::FolderOpen)
                        .label("Choose PDF…")
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.prompt_attach_pdf(choose_id.clone(), window, cx)
                        })),
                )
                .child(
                    div()
                        .mt_2()
                        .text_xs()
                        .text_color(hint)
                        .child("You can also do this later: the paper shows “No PDF” in your library until then."),
                )
        };

        panel
            .child(
                Button::new("show-in-library")
                    .outline()
                    .w_full()
                    .mt_5()
                    .icon(IconName::BookOpen)
                    .label("Show in library")
                    .on_click(open),
            )
            .into_any_element()
    }
}

/// An editable draft from Google Scholar's BibTeX (or, if that couldn't be
/// fetched, from the search result itself).
fn build_draft(
    ix: usize,
    result: &SearchResult,
    fetched: Result<String, String>,
    window: &mut Window,
    cx: &mut Context<PaprApp>,
) -> Draft {
    let parsed = fetched.and_then(|text| {
        bibtex::parse(&text)
            .map_err(|e| format!("its BibTeX couldn't be read: {e}"))?
            .into_iter()
            .next()
            .ok_or_else(|| "it returned no BibTeX entry".to_string())
    });
    let (mut entry, fetch_error) = match parsed {
        Ok(entry) => (entry, None),
        Err(err) => (fallback_entry(result), Some(err)),
    };

    // Scholar's BibTeX has no abstract or links; add what the search found.
    if entry.get("abstract").is_none() && !result.abstract_.is_empty() {
        entry.set("abstract", escape_braces(&result.abstract_));
    }
    if entry.get("url").is_none() && !result.pub_url.is_empty() {
        entry.set("url", result.pub_url.clone());
    }
    if entry.get("pdf").is_none() && !result.eprint_url.is_empty() {
        entry.set("pdf", result.eprint_url.clone());
    }

    // Order: title, author, year, the rest, then abstract and links.
    let mut ordered: Vec<(String, String)> = Vec::new();
    for name in LEADING_FIELDS {
        ordered.push((name.into(), entry.get(name).unwrap_or_default().to_string()));
    }
    for (name, value) in &entry.fields {
        if !LEADING_FIELDS.contains(&name.as_str()) && !TRAILING_FIELDS.contains(&name.as_str()) {
            ordered.push((name.clone(), value.clone()));
        }
    }
    for name in TRAILING_FIELDS {
        ordered.push((name.into(), entry.get(name).unwrap_or_default().to_string()));
    }

    let fields = ordered
        .into_iter()
        .map(|(name, value)| {
            let input = if name == "abstract" {
                FieldInput::Text(cx.new(|cx| {
                    let mut state = TextareaState::new(window, cx).auto_grow(3, 12);
                    state.set_value(value, window, cx);
                    state
                }))
            } else {
                FieldInput::Line(cx.new(|cx| InputState::new(window, cx).default_value(value)))
            };
            DraftField { name, input }
        })
        .collect();

    Draft {
        result: ix,
        fetch_error,
        kind: cx.new(|cx| InputState::new(window, cx).default_value(entry.kind.clone())),
        key: cx.new(|cx| InputState::new(window, cx).default_value(entry.key.clone())),
        fields,
        collections: BTreeSet::new(),
        saved: None,
    }
}

/// An entry built from the search result alone.
fn fallback_entry(result: &SearchResult) -> Entry {
    let mut fields = vec![("title".to_string(), escape_braces(&result.title))];
    if !result.authors.is_empty() {
        fields.push(("author".into(), escape_braces(&result.authors.join(" and "))));
    }
    if !result.year.is_empty() {
        fields.push(("year".into(), result.year.clone()));
    }
    // Scholar truncates long venues with "…"; don't keep a truncated one.
    if !result.venue.is_empty() && !result.venue.contains('…') {
        fields.push(("journal".into(), escape_braces(&result.venue)));
    }
    Entry {
        kind: "article".into(),
        key: citation_key(result),
        fields,
    }
}

/// A Google-Scholar-style key: first author's surname, year, first title word.
fn citation_key(result: &SearchResult) -> String {
    let word = |s: &str| -> String {
        s.chars()
            .filter(|c| c.is_alphanumeric())
            .flat_map(char::to_lowercase)
            .collect()
    };
    let surname = result
        .authors
        .first()
        .and_then(|a| a.split_whitespace().last())
        .map(word)
        .unwrap_or_default();
    let title_word = result
        .title
        .split_whitespace()
        .map(word)
        .find(|w| w.len() > 3 && !["with", "from", "this", "that", "into", "towards"].contains(&w.as_str()))
        .unwrap_or_default();
    let key = format!("{surname}{}{title_word}", result.year);
    if key.is_empty() { "paper".into() } else { key }
}

/// Text from the web as a safe BibTeX value.
fn escape_braces(text: &str) -> String {
    text.replace('{', "\\{").replace('}', "\\}")
}

#[cfg(test)]
mod tests {
    use super::{citation_key, escape_braces, fallback_entry};
    use crate::scholar::SearchResult;

    fn result() -> SearchResult {
        SearchResult {
            title: "Attention is all you need".into(),
            authors: vec!["A Vaswani".into(), "N Shazeer".into()],
            year: "2017".into(),
            venue: "Advances in neural …".into(),
            abstract_: "We use {braces}".into(),
            pub_url: String::new(),
            eprint_url: String::new(),
            num_citations: 1,
            raw: serde_json::Value::Null,
        }
    }

    #[test]
    fn builds_a_valid_entry_from_a_search_result() {
        let entry = fallback_entry(&result());
        assert_eq!(entry.key, "vaswani2017attention");
        assert_eq!(entry.get("author"), Some("A Vaswani and N Shazeer"));
        assert!(entry.get("journal").is_none(), "truncated venues are dropped");
        assert!(entry.validate().is_ok());
        assert_eq!(citation_key(&result()), "vaswani2017attention");
        assert!(crate::bibtex::check_value(&escape_braces("a } b {")).is_ok());
    }
}
