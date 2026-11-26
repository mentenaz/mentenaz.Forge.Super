//! Registry search and package-details fetching: `search_npm` / `fetch_details`
//! run on the panel's context and store results on `NpmManagerPanel`.

use std::sync::Arc;

use futures::io::AsyncReadExt as _;
use gpui::{AsyncApp, Context, WeakEntity};

use http_client::HttpClient;
use npm_backend::{
    NpmSearchResult, node_engine_compatible, parse_npm_downloads, parse_npm_packument,
    parse_npm_search,
};

use crate::NpmManagerPanel;

const SEARCH_URL: &str = "https://registry.npmjs.org/-/v1/search";

impl NpmManagerPanel {
    pub(crate) fn search_npm(&mut self, cx: &mut Context<Self>) {
        let query = self.search_input.read(cx).value().trim().to_string();
        if query.is_empty() {
            self.search_results.clear();
            self.search_total = 0;
            self.search_error = None;
            self.search_loading = false;
            cx.notify();
            return;
        }
        self.search_loading = true;
        self.search_error = None;
        cx.notify();

        let http_client = cx.http_client();
        let active = self.active();
        let node_version = active.engine.as_ref().map(|e| e.node_version.clone());
        let page = self.search_page;
        let url = format!(
            "{SEARCH_URL}?text={}&size=25&from={}",
            urlencoding::encode(&query),
            page * 25
        );
        let this = cx.weak_entity();

        cx.spawn(async move |_: WeakEntity<Self>, cx: &mut AsyncApp| {
            let outcome = fetch_search(&http_client, &url, node_version.as_deref()).await;
            let _ = cx.update(|app| {
                if let Some(panel) = this.upgrade() {
                    let _ = panel.update(app, |panel, cx| {
                        panel.search_loading = false;
                        match outcome {
                            Ok((results, total)) => {
                                if panel.search_page == 0 {
                                    panel.search_results = results;
                                } else {
                                    panel.search_results.extend(results);
                                }
                                panel.search_total = total;
                            }
                            Err(e) => panel.search_error = Some(e),
                        }
                        cx.notify();
                    });
                }
            });
        })
        .detach();
    }

    pub(crate) fn fetch_details(&mut self, name: String, cx: &mut Context<Self>) {
        self.fetch_details_with_mode(name, false, cx);
    }

    /// Like [`Self::fetch_details`] but opens the details pane straight to the
    /// markdown-rendered README (when the packument has one) — the search
    /// card's "More info" button.
    pub(crate) fn fetch_details_and_readme(&mut self, name: String, cx: &mut Context<Self>) {
        self.fetch_details_with_mode(name, true, cx);
    }

    fn fetch_details_with_mode(
        &mut self,
        name: String,
        open_readme_on_load: bool,
        cx: &mut Context<Self>,
    ) {
        self.selected = Some(name.clone());
        self.details = None;
        self.details_error = None;
        self.details_loading = true;
        self.show_readme = false;
        self.open_readme_on_load = open_readme_on_load;
        cx.notify();

        let http_client = cx.http_client();
        let packument_url = format!("https://registry.npmjs.org/{}", urlencoding::encode(&name));
        let downloads_url = format!(
            "https://api.npmjs.org/downloads/point/last-week/{}",
            urlencoding::encode(&name)
        );
        let this = cx.weak_entity();

        cx.spawn(async move |_: WeakEntity<Self>, cx: &mut AsyncApp| {
            let packument = fetch_json(&http_client, &packument_url).await;
            let downloads = fetch_json(&http_client, &downloads_url).await;
            let _ = cx.update(|app| {
                if let Some(panel) = this.upgrade() {
                    let _ = panel.update(app, |panel, cx| {
                        panel.details_loading = false;
                        match packument.and_then(|json| parse_npm_packument(&json)) {
                            Ok(mut details) => {
                                details.weekly_downloads =
                                    downloads.ok().and_then(|json| parse_npm_downloads(&json));
                                let open_readme =
                                    panel.open_readme_on_load && details.readme.is_some();
                                panel.show_readme = open_readme;
                                panel.open_readme_on_load = false;
                                panel.details = Some(details);
                            }
                            Err(e) => panel.details_error = Some(e),
                        }
                        cx.notify();
                    });
                }
            });
        })
        .detach();
    }
}

async fn fetch_search(
    http_client: &Arc<dyn HttpClient>,
    url: &str,
    node_version: Option<&str>,
) -> Result<(Vec<NpmSearchResult>, usize), String> {
    let json = fetch_json(http_client, url).await?;
    let (mut results, total) = parse_npm_search(&json);
    for result in &mut results {
        if let Some(range) = result.engines_node.as_deref() {
            result.compat = node_version.and_then(|v| node_engine_compatible(v, range));
        }
    }
    Ok((results, total))
}

pub(crate) async fn fetch_json(
    http_client: &Arc<dyn HttpClient>,
    url: &str,
) -> Result<serde_json::Value, String> {
    let mut response = http_client
        .get(url, Default::default(), true)
        .await
        .map_err(|e| format!("request failed: {e}"))?;
    if !response.status().is_success() {
        return Err(format!("HTTP {}", response.status()));
    }
    let mut body = Vec::new();
    response
        .body_mut()
        .read_to_end(&mut body)
        .await
        .map_err(|e| e.to_string())?;
    serde_json::from_slice(&body).map_err(|e| format!("invalid JSON: {e}"))
}
