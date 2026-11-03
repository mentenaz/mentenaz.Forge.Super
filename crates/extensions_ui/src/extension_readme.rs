//! A tab that shows an extension's README, read from the registry, so what an
//! extension does can be read before it is installed.
//!
//! A README is written by the extension's authors, so it is treated as untrusted:
//! the Markdown renderer never runs anything, links open in the browser only when
//! they are web or mail addresses, and an image is loaded only when GitHub hosts it,
//! so opening a README does not hand your address to an arbitrary image server.

use std::sync::Arc;

use extension_host::{ExtensionReadme, ExtensionStore};
use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, ImageSource, IntoElement,
    ParentElement, Render, Resource, SharedString, SharedUri, Styled, Task, Window,
};
use language::LanguageRegistry;
use markdown::{
    CodeBlockRenderer, CopyButtonVisibility, Markdown, MarkdownElement, MarkdownFont,
    MarkdownOptions, MarkdownStyle, WrapButtonVisibility,
};
use ui::{CommonAnimationExt, prelude::*};
use url::Url;
use workspace::{
    Workspace,
    item::{Item, ItemEvent},
};

enum ReadmeState {
    Loading,
    /// Nothing to show, with the reason.
    Unavailable(SharedString),
    Loaded {
        markdown: Entity<Markdown>,
        /// What relative images in the README resolve against.
        raw_base: Option<Url>,
        /// What relative links in the README resolve against.
        blob_base: Option<Url>,
    },
}

pub struct ExtensionReadmeView {
    extension_id: Arc<str>,
    name: SharedString,
    state: ReadmeState,
    focus_handle: FocusHandle,
    _load: Task<()>,
}

impl ExtensionReadmeView {
    /// Opens the README of `extension_id` in a tab, or brings the tab forward if
    /// it is already open.
    pub fn open(
        workspace: &mut Workspace,
        extension_id: Arc<str>,
        name: SharedString,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let existing = workspace
            .items_of_type::<Self>(cx)
            .find(|view| view.read(cx).extension_id == extension_id);
        if let Some(existing) = existing {
            workspace.activate_item(&existing, true, true, window, cx);
            return;
        }

        let language_registry = workspace.app_state().languages.clone();
        let view = cx.new(|cx| Self::new(extension_id, name, language_registry, window, cx));
        workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
    }

    fn new(
        extension_id: Arc<str>,
        name: SharedString,
        language_registry: Arc<LanguageRegistry>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let fetch = ExtensionStore::global(cx).update(cx, |store, cx| {
            store.fetch_extension_readme(&extension_id, cx)
        });

        let load = cx.spawn_in(window, async move |this, cx| {
            let result = fetch.await;
            this.update_in(cx, |this, _window, cx| {
                this.finish_loading(result, language_registry, cx)
            })
            .ok();
        });

        Self {
            extension_id,
            name,
            state: ReadmeState::Loading,
            focus_handle: cx.focus_handle(),
            _load: load,
        }
    }

    fn finish_loading(
        &mut self,
        result: anyhow::Result<ExtensionReadme>,
        language_registry: Arc<LanguageRegistry>,
        cx: &mut Context<Self>,
    ) {
        self.state = match result {
            Err(error) => {
                log::warn!(
                    "could not load the README of {}: {error:#}",
                    self.extension_id
                );
                ReadmeState::Unavailable(
                    "The README could not be loaded. Check your connection, then open it again."
                        .into(),
                )
            }
            Ok(ExtensionReadme {
                readme: Some(text),
                readme_raw_base,
                readme_blob_base,
            }) if !text.trim().is_empty() => {
                let markdown = cx.new(|cx| {
                    Markdown::new_with_options(
                        text.into(),
                        Some(language_registry),
                        None,
                        MarkdownOptions {
                            parse_html: true,
                            ..Default::default()
                        },
                        cx,
                    )
                });
                ReadmeState::Loaded {
                    markdown,
                    raw_base: parse_base(readme_raw_base.as_deref()),
                    blob_base: parse_base(readme_blob_base.as_deref()),
                }
            }
            Ok(ExtensionReadme {
                readme: Some(_), ..
            }) => ReadmeState::Unavailable("This extension has no README.".into()),
            Ok(_) => ReadmeState::Unavailable(
                "The registry has no README for this extension yet.".into(),
            ),
        };
        cx.notify();
    }
}

/// A base address is only used when it is a plain https address.
fn parse_base(base: Option<&str>) -> Option<Url> {
    Url::parse(base?).ok().filter(|url| url.scheme() == "https")
}

fn resolve(target: &str, base: Option<&Url>) -> Option<Url> {
    match base {
        Some(base) => base.join(target).ok(),
        None => Url::parse(target).ok(),
    }
}

fn is_github_host(host: &str) -> bool {
    host == "github.com"
        || host == "githubusercontent.com"
        || host.ends_with(".githubusercontent.com")
}

/// An image is shown only when GitHub hosts it. Anything else is not loaded.
fn resolve_image(source: &str, base: Option<&Url>) -> Option<ImageSource> {
    let url = resolve(source, base)?;
    if url.scheme() != "https" || !is_github_host(url.host_str()?) {
        return None;
    }
    Some(ImageSource::Resource(Resource::Uri(SharedUri::from(
        url.to_string(),
    ))))
}

/// The address a link should open, or `None` when it must not be opened: only web,
/// mail and phone addresses are.
fn link_to_open(target: &str, base: Option<&Url>) -> Option<Url> {
    if target.starts_with('#') {
        // Anchors inside the README have nowhere to go in this view.
        return None;
    }
    let url = resolve(target, base)?;
    matches!(url.scheme(), "https" | "http" | "mailto" | "tel").then_some(url)
}

impl Focusable for ExtensionReadmeView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<ItemEvent> for ExtensionReadmeView {}

impl Item for ExtensionReadmeView {
    type Event = ItemEvent;

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        format!("README: {}", self.name).into()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::FileDoc))
    }

    fn show_toolbar(&self) -> bool {
        false
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }
}

impl Render for ExtensionReadmeView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = match &self.state {
            ReadmeState::Loading => h_flex()
                .gap_2()
                .child(
                    Icon::new(IconName::ArrowCircle)
                        .size(IconSize::Small)
                        .color(Color::Muted)
                        .with_rotate_animation(2),
                )
                .child(Label::new("Loading the README…").color(Color::Muted))
                .into_any_element(),
            ReadmeState::Unavailable(message) => Label::new(message.clone())
                .color(Color::Muted)
                .into_any_element(),
            ReadmeState::Loaded {
                markdown,
                raw_base,
                blob_base,
            } => {
                let style = MarkdownStyle::themed(MarkdownFont::Preview, window, cx);
                let raw_base = raw_base.clone();
                let blob_base = blob_base.clone();
                MarkdownElement::new(markdown.clone(), style)
                    .code_block_renderer(CodeBlockRenderer::Default {
                        copy_button_visibility: CopyButtonVisibility::VisibleOnHover,
                        wrap_button_visibility: WrapButtonVisibility::Hidden,
                        border: false,
                    })
                    .image_resolver(move |source, _cx| resolve_image(source, raw_base.as_ref()))
                    .on_url_click(move |target, _window, cx| {
                        if let Some(url) = link_to_open(&target, blob_base.as_ref()) {
                            cx.open_url(url.as_str());
                        }
                    })
                    .into_any_element()
            }
        };

        v_flex()
            .id("extension-readme")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .child(
                div()
                    .id("extension-readme-scroll")
                    .size_full()
                    .overflow_y_scroll()
                    .child(
                        v_flex()
                            .w_full()
                            .max_w(rems(50.))
                            .mx_auto()
                            .p_6()
                            .gap_4()
                            .child(
                                v_flex()
                                    .gap_1()
                                    .child(Label::new(self.name.clone()).size(LabelSize::Large))
                                    .child(
                                        Label::new(
                                            "README, written by the extension's authors and not by Mentenaz.",
                                        )
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                    ),
                            )
                            .child(body),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base(url: &str) -> Url {
        Url::parse(url).unwrap()
    }

    #[test]
    fn images_resolve_against_the_raw_base() {
        let raw = base("https://raw.githubusercontent.com/o/r/abc123/ext/");
        assert!(resolve_image("images/preview.png", Some(&raw)).is_some());
        assert!(resolve_image("../shared/logo.png", Some(&raw)).is_some());
    }

    #[test]
    fn only_github_hosted_https_images_load() {
        let raw = base("https://raw.githubusercontent.com/o/r/abc123/");
        assert!(
            resolve_image("https://github.com/user-attachments/assets/1", Some(&raw)).is_some()
        );
        assert!(resolve_image("https://camo.githubusercontent.com/x", Some(&raw)).is_some());
        assert!(resolve_image("https://img.shields.io/badge/x-y-green", Some(&raw)).is_none());
        assert!(resolve_image("http://github.com/x.png", Some(&raw)).is_none());
        assert!(resolve_image("https://github.com.evil.example/x.png", Some(&raw)).is_none());
        assert!(resolve_image("https://evilgithubusercontent.com/x.png", Some(&raw)).is_none());
        assert!(resolve_image("data:image/png;base64,AAAA", Some(&raw)).is_none());
    }

    #[test]
    fn a_relative_image_with_no_base_is_not_loaded() {
        assert!(resolve_image("images/preview.png", None).is_none());
    }

    #[test]
    fn links_resolve_against_the_blob_base() {
        let blob = base("https://github.com/o/r/blob/abc123/ext/");
        assert_eq!(
            link_to_open("docs/usage.md", Some(&blob)).unwrap().as_str(),
            "https://github.com/o/r/blob/abc123/ext/docs/usage.md"
        );
        assert_eq!(
            link_to_open("https://example.com/a", Some(&blob))
                .unwrap()
                .as_str(),
            "https://example.com/a"
        );
    }

    #[test]
    fn only_web_mail_and_phone_links_open() {
        let blob = base("https://github.com/o/r/blob/abc123/");
        assert!(link_to_open("mailto:a@example.com", Some(&blob)).is_some());
        assert!(link_to_open("tel:+27123456789", Some(&blob)).is_some());
        assert!(link_to_open("http://example.com", Some(&blob)).is_some());
        assert!(link_to_open("javascript:alert(1)", Some(&blob)).is_none());
        assert!(link_to_open("file:///etc/passwd", Some(&blob)).is_none());
        assert!(link_to_open("data:text/html,<script>", Some(&blob)).is_none());
        assert!(link_to_open("zed://settings", Some(&blob)).is_none());
        assert!(link_to_open("#installation", Some(&blob)).is_none());
    }

    #[test]
    fn a_base_must_be_https() {
        assert!(parse_base(Some("https://raw.githubusercontent.com/o/r/abc/")).is_some());
        assert!(parse_base(Some("http://example.com/")).is_none());
        assert!(parse_base(Some("not a url")).is_none());
        assert!(parse_base(None).is_none());
    }
}
