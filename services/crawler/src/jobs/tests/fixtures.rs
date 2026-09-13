// Provides in-memory test doubles for crawl-job behavior tests.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use sea_orm::DbErr;

use super::*;
use crate::store::Saved;

#[derive(Clone, Default)]
pub(super) struct MemoryPages(pub(super) Arc<Mutex<Vec<String>>>);

impl PageStore for MemoryPages {
    async fn save(&self, page: CountedPage) -> Result<Saved, DbErr> {
        self.0.lock().unwrap().push(page.url);
        Ok(Saved {
            title: String::new(),
            main_text: String::new(),
            changed: true,
        })
    }
}

#[derive(Clone)]
pub(super) struct UnchangedPages;

impl PageStore for UnchangedPages {
    async fn save(&self, _page: CountedPage) -> Result<Saved, DbErr> {
        Ok(Saved {
            title: "Title".to_owned(),
            main_text: "Content".to_owned(),
            changed: false,
        })
    }
}

#[derive(Clone)]
pub(super) struct FailingPages;

impl PageStore for FailingPages {
    async fn save(&self, _page: CountedPage) -> Result<Saved, DbErr> {
        Err(DbErr::Custom("database is down".into()))
    }
}

#[derive(Clone, Default)]
pub(super) struct NoopKnowledgeBase;

impl KnowledgeBase for NoopKnowledgeBase {
    async fn ingest(&self, _url: &str, _title: &str, _content: &str) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Default)]
pub(super) struct RecordingKnowledgeBase {
    pub(super) ingested: Arc<Mutex<Vec<String>>>,
    fails: bool,
}

impl RecordingKnowledgeBase {
    pub(super) fn failing() -> Self {
        Self {
            fails: true,
            ..Self::default()
        }
    }
}

impl KnowledgeBase for RecordingKnowledgeBase {
    async fn ingest(&self, url: &str, _title: &str, _content: &str) -> Result<(), String> {
        if self.fails {
            return Err("knowledge-base is unavailable".to_owned());
        }
        self.ingested.lock().unwrap().push(url.to_owned());
        Ok(())
    }
}

type KeyedJob = (CrawlJob, Option<String>);

#[derive(Clone, Default)]
pub(super) struct MemoryJobs {
    rows: Arc<Mutex<HashMap<i64, KeyedJob>>>,
}

impl JobStore for MemoryJobs {
    async fn create(&self, base_url: &str, key: Option<&str>) -> Result<CrawlJob, DbErr> {
        let mut rows = self.rows.lock().unwrap();
        if key.is_some() && rows.values().any(|(_, k)| k.as_deref() == key) {
            return Err(DbErr::Custom("duplicate idempotency key".into()));
        }
        let job = CrawlJob {
            id: rows.len() as i64 + 1,
            base_url: base_url.to_owned(),
            status: CrawlStatus::Queued,
            pages_crawled: 0,
            pages_skipped: 0,
        };
        rows.insert(job.id, (job.clone(), key.map(str::to_owned)));
        Ok(job)
    }

    async fn find_by_key(&self, key: &str) -> Result<Option<CrawlJob>, DbErr> {
        Ok(self
            .rows
            .lock()
            .unwrap()
            .values()
            .find(|(_, stored_key)| stored_key.as_deref() == Some(key))
            .map(|(job, _)| job.clone()))
    }

    async fn get(&self, id: i64) -> Result<Option<CrawlJob>, DbErr> {
        Ok(self
            .rows
            .lock()
            .unwrap()
            .get(&id)
            .map(|(job, _)| job.clone()))
    }

    async fn set_status(&self, id: i64, status: CrawlStatus) -> Result<(), DbErr> {
        if let Some((job, _)) = self.rows.lock().unwrap().get_mut(&id) {
            job.status = status;
        }
        Ok(())
    }

    async fn add_crawled_page(&self, id: i64) -> Result<(), DbErr> {
        if let Some((job, _)) = self.rows.lock().unwrap().get_mut(&id) {
            job.pages_crawled += 1;
        }
        Ok(())
    }

    async fn fail_unfinished(&self) -> Result<u64, DbErr> {
        Ok(0)
    }
}

#[derive(Clone, Default)]
pub(super) struct NoopEdges;

impl EdgeStore for NoopEdges {
    async fn replace_outbound(
        &self,
        _from_url: &str,
        _links: Vec<crate::links::Link>,
    ) -> Result<(), DbErr> {
        Ok(())
    }

    async fn neighbors(
        &self,
        _start_url: &str,
        _relation_types: Vec<crate::entity::page_edge::RelationType>,
        _max_depth: u32,
    ) -> Result<Vec<crate::graph::Neighbor>, DbErr> {
        Ok(Vec::new())
    }
}

type ReplacedEdges = Vec<(String, Vec<crate::links::Link>)>;

#[derive(Clone, Default)]
pub(super) struct RecordingEdges {
    pub(super) replaced: Arc<Mutex<ReplacedEdges>>,
}

impl EdgeStore for RecordingEdges {
    async fn replace_outbound(
        &self,
        from_url: &str,
        links: Vec<crate::links::Link>,
    ) -> Result<(), DbErr> {
        self.replaced
            .lock()
            .unwrap()
            .push((from_url.to_owned(), links));
        Ok(())
    }

    async fn neighbors(
        &self,
        _start_url: &str,
        _relation_types: Vec<crate::entity::page_edge::RelationType>,
        _max_depth: u32,
    ) -> Result<Vec<crate::graph::Neighbor>, DbErr> {
        Ok(Vec::new())
    }
}

#[derive(Clone, Default)]
pub(super) struct FailingEdges;

impl EdgeStore for FailingEdges {
    async fn replace_outbound(
        &self,
        _from_url: &str,
        _links: Vec<crate::links::Link>,
    ) -> Result<(), DbErr> {
        Err(DbErr::Custom("graph store is down".into()))
    }

    async fn neighbors(
        &self,
        _start_url: &str,
        _relation_types: Vec<crate::entity::page_edge::RelationType>,
        _max_depth: u32,
    ) -> Result<Vec<crate::graph::Neighbor>, DbErr> {
        Ok(Vec::new())
    }
}

pub(super) fn in_memory(
    pages: impl PageStore,
) -> Jobs<impl PageStore, MemoryJobs, NoopKnowledgeBase, NoopEdges> {
    Jobs::new(
        pages,
        MemoryJobs::default(),
        NoopKnowledgeBase,
        NoopEdges,
        ONE_AT_A_TIME,
    )
}

pub(super) fn in_memory_with_edges(
    pages: impl PageStore,
    edges: impl EdgeStore,
) -> Jobs<impl PageStore, MemoryJobs, NoopKnowledgeBase, impl EdgeStore> {
    Jobs::new(
        pages,
        MemoryJobs::default(),
        NoopKnowledgeBase,
        edges,
        ONE_AT_A_TIME,
    )
}

pub(super) fn in_memory_with(
    pages: impl PageStore,
    knowledge_base: impl KnowledgeBase,
) -> Jobs<impl PageStore, MemoryJobs, impl KnowledgeBase, NoopEdges> {
    Jobs::new(
        pages,
        MemoryJobs::default(),
        knowledge_base,
        NoopEdges,
        ONE_AT_A_TIME,
    )
}
