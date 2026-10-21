//! Providers whose statements are not SQL.
//!
//! Violet generates SQL for data views, paging, counting and the tree. A
//! provider for another language says so in [`Provider::extras`], and then
//! lists, pages and counts itself through [`Session::entries`],
//! [`Session::page`] and [`Session::count`].
//!
//! [`Provider::extras`]: crate::Provider::extras
//! [`Session::entries`]: crate::Session::entries
//! [`Session::page`]: crate::Session::page
//! [`Session::count`]: crate::Session::count

pub use crate::bindings::exports::violet::provider::data::{
    Entry, EntryPage, Language, Login, ProviderExtras,
};

impl Default for ProviderExtras {
    fn default() -> Self {
        Self {
            language: Language::Sql,
            user: Login::Required,
            password: Login::Optional,
            schema_label: String::new(),
        }
    }
}

impl Entry {
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            subtype: None,
            detail: None,
            size: None,
        }
    }
}
