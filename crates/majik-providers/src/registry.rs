//! The provider registry. Built-in descriptors are `'static` (lazily constructed).

use std::sync::{Mutex, OnceLock};

use crate::descriptor::ProviderDescriptor;
use crate::ProviderId;

pub struct ProviderRegistry {
    descriptors: Mutex<Vec<&'static ProviderDescriptor>>,
}

impl ProviderRegistry {
    /// The process-wide registry with fal, OpenRouter, Replicate and Mock registered.
    pub fn shared() -> &'static ProviderRegistry {
        static SHARED: OnceLock<ProviderRegistry> = OnceLock::new();
        SHARED.get_or_init(|| ProviderRegistry::new(true))
    }

    pub fn new(bootstrap_built_ins: bool) -> Self {
        let reg = Self { descriptors: Mutex::new(Vec::new()) };
        if bootstrap_built_ins {
            reg.register(crate::fal::descriptor());
            reg.register(crate::openrouter::descriptor());
            reg.register(crate::replicate::descriptor());
            reg.register(crate::mock::descriptor());
        }
        reg
    }

    pub fn register(&self, descriptor: &'static ProviderDescriptor) {
        let mut d = self.descriptors.lock().unwrap();
        d.retain(|x| x.id != descriptor.id);
        d.push(descriptor);
    }

    pub fn descriptor(&self, id: &ProviderId) -> Option<&'static ProviderDescriptor> {
        self.descriptors.lock().unwrap().iter().copied().find(|d| &d.id == id)
    }

    pub fn all(&self) -> Vec<&'static ProviderDescriptor> {
        self.descriptors.lock().unwrap().clone()
    }

    /// User-selectable providers sorted by display name.
    pub fn user_selectable(&self) -> Vec<&'static ProviderDescriptor> {
        let mut v: Vec<_> = self.all().into_iter().filter(|d| d.is_user_selectable).collect();
        v.sort_by(|a, b| a.display_name.cmp(b.display_name));
        v
    }
}
