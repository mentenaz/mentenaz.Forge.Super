//! The repository chooser of the publish dialog, and later of the clone
//! dialog: a search box over one owner's repositories, shown a page at a
//! time with a radio button per row, and a line naming the chosen one.
//!
//! The owner's repositories are loaded in one go (`gh_get_repos`), so
//! searching and paging only slice what is already here, the way the Helm
//! repository list does. The selection is kept by repository id, so it
//! survives searching and paging.

use gpui::AnyElement;
use gpui_component::pagination::Pagination;
use gpui_component::radio::Radio;

use super::repository_modal::HelmRepositoryModal;
use super::*;

/// Repositories shown on one page of the picker.
pub(super) const PICKER_PAGE_SIZE: u32 = 8;

pub(super) enum PickerStatus {
    Loading,
    Loaded,
    Failed(String),
}

pub(super) struct RepoPicker {
    search: Entity<InputState>,
    repos: Vec<Repo>,
    status: PickerStatus,
    page: u32,
    selected: Option<u64>,
    _search_sub: Subscription,
}

impl RepoPicker {
    pub(super) fn new(window: &mut Window, cx: &mut Context<HelmRepositoryModal>) -> Self {
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Search repositories…"));
        // A new search starts from its first page.
        let search_sub = cx.subscribe(&search, |this, _, event: &InputEvent, cx| {
            if let InputEvent::Change = event {
                this.picker.page = 1;
                cx.notify();
            }
        });
        Self {
            search,
            repos: Vec::new(),
            status: PickerStatus::Loading,
            page: 1,
            selected: None,
            _search_sub: search_sub,
        }
    }

    /// Empties the list for a new owner: the choice of the old one no longer
    /// applies.
    pub(super) fn start_loading(&mut self) {
        self.repos.clear();
        self.status = PickerStatus::Loading;
        self.page = 1;
        self.selected = None;
    }

    pub(super) fn finish(&mut self, result: Result<Vec<Repo>, String>) {
        match result {
            Ok(repos) => {
                self.repos = repos;
                self.status = PickerStatus::Loaded;
            }
            Err(error) => self.status = PickerStatus::Failed(error),
        }
    }

    /// The repository chosen, if any.
    pub(super) fn selected_repo(&self) -> Option<Repo> {
        let id = self.selected?;
        self.repos.iter().find(|repo| repo.id == id).cloned()
    }

    pub(super) fn render(&self, cx: &mut Context<HelmRepositoryModal>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let danger = cx.theme().danger;
        let query = self.search.read(cx).value().to_string();
        let matching = repos::matching_repos(&self.repos, &query);
        let (page, last_page, rows) = page_slice(matching.len(), self.page, PICKER_PAGE_SIZE);

        let body = match &self.status {
            PickerStatus::Loading => h_flex()
                .gap_2()
                .items_center()
                .child(Spinner::new().small())
                .child(div().text_sm().text_color(muted).child("Loading repositories…"))
                .into_any_element(),
            PickerStatus::Failed(error) => div()
                .text_xs()
                .text_color(danger)
                .child(format!("Could not load repositories: {error}"))
                .into_any_element(),
            PickerStatus::Loaded if matching.is_empty() => div()
                .text_sm()
                .text_color(muted)
                .child(if self.repos.is_empty() {
                    "This owner has no repositories yet."
                } else {
                    "No repositories match your search."
                })
                .into_any_element(),
            PickerStatus::Loaded => v_flex()
                .gap_1()
                .children(matching[rows].iter().map(|&ix| {
                    let repo = &self.repos[ix];
                    let id = repo.id;
                    let mut label = repo.name.clone();
                    if repo.private {
                        label.push_str("  ·  private");
                    }
                    v_flex()
                        .child(
                            Radio::new(("helm-picker-repo", ix))
                                .label(label)
                                .checked(self.selected == Some(id))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.picker.selected = Some(id);
                                    cx.notify();
                                })),
                        )
                        .children(
                            repo.description
                                .clone()
                                .filter(|description| !description.is_empty())
                                .map(|description| {
                                    div()
                                        .pl_6()
                                        .text_xs()
                                        .text_color(muted)
                                        .truncate()
                                        .child(description)
                                }),
                        )
                }))
                .into_any_element(),
        };

        let summary = self.selected_repo().map(|repo| {
            div()
                .text_sm()
                .child(format!("Owner: {}   Repo: {}", repo.owner.login, repo.name))
        });

        v_flex()
            .gap_2()
            .child(Input::new(&self.search))
            .child(body)
            .when(last_page > 1, |column| {
                column.child(
                    Pagination::new("helm-picker-pages")
                        .current_page(page as usize)
                        .total_pages(last_page as usize)
                        .compact()
                        .on_click(cx.listener(|this, page: &usize, _, cx| {
                            this.picker.page = *page as u32;
                            cx.notify();
                        })),
                )
            })
            .children(summary)
            .into_any_element()
    }
}
