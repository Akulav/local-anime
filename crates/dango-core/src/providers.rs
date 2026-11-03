//! Loads anime providers from the remote registry into embedded JS runtimes.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use parking_lot::RwLock;
use serde::Serialize;

use crate::http::Http;
use crate::js::{JsProvider, ProviderCaps};
use crate::registry::{ProviderEntry, ProxyRule, RegistryStore};

pub struct Provider {
    pub entry: ProviderEntry,
    pub js: Arc<JsProvider>,
}

impl Provider {
    pub fn id(&self) -> &str {
        &self.js.id
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogItem {
    pub id: String,
    pub label: String,
    pub version: String,
    pub mature: bool,
    pub sub: Option<String>,
    pub tier: Option<String>,
    pub modes: Vec<String>,
    pub loaded: bool,
    pub error: Option<String>,
    pub caps: Option<ProviderCaps>,
}

#[derive(Default)]
struct State {
    map: HashMap<String, Arc<Provider>>,
    catalog: Vec<CatalogItem>,
    rules: Vec<ProxyRule>,
    loaded_at: i64,
}

pub struct Providers {
    dir: PathBuf,
    http: Http,
    app_version: String,
    state: RwLock<State>,
    refresh_lock: tokio::sync::Mutex<()>,
}

fn catalog_item(e: &ProviderEntry, caps: Option<ProviderCaps>, error: Option<String>) -> CatalogItem {
    CatalogItem {
        id: e.id.to_lowercase(),
        label: e.label.clone(),
        version: e.version.clone(),
        mature: e.mature,
        sub: e.sub.clone(),
        tier: e.tier.clone(),
        modes: e.modes.clone().unwrap_or_else(|| vec!["sub".into(), "dub".into()]),
        loaded: caps.is_some(),
        error,
        caps,
    }
}

impl Providers {
    pub fn new(dir: PathBuf, http: Http, app_version: &str) -> Arc<Self> {
        Arc::new(Self {
            dir,
            http,
            app_version: app_version.to_string(),
            state: RwLock::new(State::default()),
            refresh_lock: tokio::sync::Mutex::new(()),
        })
    }

    pub fn get(&self, id: &str) -> Option<Arc<Provider>> {
        self.state.read().map.get(&id.to_lowercase()).cloned()
    }

    pub fn all(&self) -> Vec<Arc<Provider>> {
        let mut v: Vec<_> = self.state.read().map.values().cloned().collect();
        v.sort_by(|a, b| a.entry.id.cmp(&b.entry.id));
        v
    }

    pub fn catalog(&self) -> Vec<CatalogItem> {
        self.state.read().catalog.clone()
    }

    pub fn rules(&self) -> Vec<ProxyRule> {
        self.state.read().rules.clone()
    }

    pub fn loaded_at(&self) -> i64 {
        self.state.read().loaded_at
    }

    pub fn rule_for(&self, url: &str) -> Option<ProxyRule> {
        let host = url::Url::parse(url).ok()?.host_str()?.to_ascii_lowercase();
        self.state.read().rules.iter().find(|r| r.matches(&host)).cloned()
    }

    /// Downloads the registry and (re)instantiates changed anime providers.
    /// Unchanged modules keep their running instance.
    pub async fn refresh(&self, registry_url: &str) -> Result<()> {
        let _guard = self.refresh_lock.lock().await;
        let store = RegistryStore::new(self.dir.clone(), registry_url.to_string(), self.http.clone());
        let outcome = store.load(&self.app_version, |p| p.kind() == "anime").await?;

        let existing: HashMap<String, Arc<Provider>> = self.state.read().map.clone();
        let loads = outcome.modules.into_iter().map(|m| {
            let key = m.entry.id.to_lowercase();
            let reuse = existing.get(&key).filter(|p| p.entry.file_name() == m.entry.file_name()).cloned();
            async move {
                if let Some(p) = reuse {
                    return (m.entry, Ok(p), m.fallback_error);
                }
                let res = JsProvider::load(&key, m.source)
                    .await
                    .map(|js| Arc::new(Provider { entry: m.entry.clone(), js: Arc::new(js) }));
                (m.entry, res, m.fallback_error)
            }
        });
        let results = futures::future::join_all(loads).await;

        let mut st = State { loaded_at: crate::store::now_secs(), ..Default::default() };
        for (entry, res, fallback) in results {
            match res {
                Ok(p) => {
                    tracing::info!(provider = %entry.id, version = %entry.version, "provider loaded");
                    st.catalog.push(catalog_item(&entry, Some(p.js.caps.clone()), fallback));
                    if let Some(r) = &entry.proxy {
                        st.rules.push(r.clone());
                    }
                    st.map.insert(entry.id.to_lowercase(), p);
                }
                Err(e) => {
                    tracing::warn!(provider = %entry.id, "provider failed: {e:#}");
                    st.catalog.push(catalog_item(&entry, None, Some(format!("{e:#}"))));
                    if let Some(r) = &entry.proxy {
                        st.rules.push(r.clone());
                    }
                }
            }
        }
        for (entry, err) in outcome.failures {
            st.catalog.push(catalog_item(&entry, None, Some(err)));
        }
        st.catalog.sort_by(|a, b| a.id.cmp(&b.id));
        *self.state.write() = st;
        Ok(())
    }
}
