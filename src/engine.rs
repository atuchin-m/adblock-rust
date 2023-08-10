//! The adblock [`Engine`] is the primary interface for adblocking.

use crate::blocker::{Blocker, BlockerError, BlockerOptions, BlockerResult};
use crate::cosmetic_filter_cache::{CosmeticFilterCache, UrlSpecificResources};
use crate::lists::{FilterSet, ParseOptions};
use crate::regex_manager::RegexManagerDiscardPolicy;
use crate::request::Request;
use crate::resources::{Resource, ResourceStorage};

use std::collections::HashSet;

/// Drives high-level blocking logic and is responsible for loading filter lists into an optimized
/// format that can be queried efficiently.
///
/// For performance optimization reasons, the [`Engine`] is not designed to have rules added or
/// removed after its initial creation. Making changes to the rules loaded is accomplished by
/// creating a new engine to replace it.
///
/// ## Usage
///
/// ### Initialization
///
/// You'll first want to combine all of your filter lists in a [`FilterSet`], which will parse list
/// header metadata. Once all lists have been composed together, you can call
/// [`Engine::from_filter_set`] to start using them for blocking.
///
/// You may also want to supply certain assets for `$redirect` filters and `##+js(...)` scriptlet
/// injections. These are known as [`Resource`]s, and can be provided with
/// [`Engine::use_resources`]. See the [`crate::resources`] module for more information.
///
/// ### Network blocking
///
/// Use the [`Engine::check_network_request`] method to determine how to handle a network request.
///
/// If you _only_ need network blocking, consider using a [`Blocker`] directly.
///
/// ### Cosmetic filtering
///
/// Call [`Engine::url_cosmetic_resources`] to determine what actions should be taken to prepare a
/// particular page before it starts loading.
///
/// Once the page has been loaded, any new CSS classes or ids that appear on the page should be passed to
/// [`Engine::hidden_class_id_selectors`] on an ongoing basis to determine additional elements that
/// should be hidden dynamically.
pub struct Engine {
    blocker: Blocker,
    cosmetic_cache: CosmeticFilterCache,
    resources: ResourceStorage,
}

impl Default for Engine {
    /// Equivalent to `Engine::new(true)`.
    fn default() -> Self {
        Self::new(true)
    }
}

#[cfg(feature = "debug-info")]
pub struct EngineDebugInfo {
    pub blocker_debug_info: crate::blocker::BlockerDebugInfo,
}

impl Engine {
    /// Creates a new adblocking `Engine`. `Engine`s created without rules should generally only be
    /// used with deserialization.
    /// - `optimize` specifies whether or not to attempt to compress the internal representation by
    /// combining similar rules.
    pub fn new(optimize: bool) -> Self {
        let blocker_options = BlockerOptions {
            enable_optimizations: optimize,
        };

        Self {
            blocker: Blocker::new(vec![], &blocker_options),
            cosmetic_cache: CosmeticFilterCache::new(),
            resources: ResourceStorage::default(),
        }
    }

    /// Loads rules in a single format, enabling optimizations and discarding debug information.
    pub fn from_rules(rules: impl IntoIterator<Item=impl AsRef<str>>, opts: ParseOptions) -> Self {
        let mut filter_set = FilterSet::new(false);
        filter_set.add_filters(rules, opts);
        Self::from_filter_set(filter_set, true)
    }

    /// Loads rules, enabling optimizations and including debug information.
    pub fn from_rules_debug(rules: impl IntoIterator<Item=impl AsRef<str>>, opts: ParseOptions) -> Self {
        Self::from_rules_parametrised(rules, opts, true, true)
    }

    pub fn from_rules_parametrised(filter_rules: impl IntoIterator<Item=impl AsRef<str>>, opts: ParseOptions, debug: bool, optimize: bool) -> Self {
        let mut filter_set = FilterSet::new(debug);
        filter_set.add_filters(filter_rules, opts);
        Self::from_filter_set(filter_set, optimize)
    }

    /// Loads rules from the given `FilterSet`. It is recommended to use a `FilterSet` when adding
    /// rules from multiple sources.
    pub fn from_filter_set(set: FilterSet, optimize: bool) -> Self {
        let FilterSet { network_filters, cosmetic_filters, .. } = set;

        let blocker_options = BlockerOptions {
            enable_optimizations: optimize,
        };

        Self {
            blocker: Blocker::new(network_filters, &blocker_options),
            cosmetic_cache: CosmeticFilterCache::from_rules(cosmetic_filters),
            resources: ResourceStorage::default(),
        }
    }

    /// Serializes the `Engine` into a binary format so that it can be quickly reloaded later.
    pub fn serialize_raw(&self) -> Result<Vec<u8>, BlockerError> {
        use crate::data_format::SerializeFormat;

        let serialize_format = SerializeFormat::build(&self.blocker, &self.cosmetic_cache);

        serialize_format.serialize().map_err(|_e| {
            BlockerError::SerializationError
        })
    }

    /// Deserialize the `Engine` from the binary format generated by `Engine::serialize_raw`. The
    /// method will automatically select the correct deserialization implementation.
    pub fn deserialize(&mut self, serialized: &[u8]) -> Result<(), BlockerError> {
        use crate::data_format::DeserializeFormat;
        let current_tags = self.blocker.tags_enabled();
        let deserialize_format = DeserializeFormat::deserialize(serialized).map_err(|_e| {
            BlockerError::DeserializationError
        })?;
        let (blocker, cosmetic_cache) = deserialize_format.build();
        self.blocker = blocker;
        self.blocker.use_tags(&current_tags.iter().map(|s| &**s).collect::<Vec<_>>());
        self.cosmetic_cache = cosmetic_cache;
        Ok(())
    }

    /// Check if a request for a network resource from `url`, of type `request_type`, initiated by
    /// `source_url`, should be blocked.
    pub fn check_network_request(&self, request: &Request) -> BlockerResult {
        self.blocker.check(request, &self.resources)
    }

    pub fn check_network_request_subset(
        &self,
        request: &Request,
        previously_matched_rule: bool,
        force_check_exceptions: bool,
    ) -> BlockerResult {
        self.blocker.check_parameterised(request, &self.resources, previously_matched_rule, force_check_exceptions)
    }

    /// Returns a string containing any additional CSP directives that should be added to this
    /// request's response. Only applies to document and subdocument requests.
    ///
    /// If multiple policies are present from different rules, they will be joined by commas.
    pub fn get_csp_directives(
        &self,
        request: &Request,
    ) -> Option<String> {
        self.blocker.get_csp_directives(request)
    }

    /// Check if a given filter has been previously added to this `Engine`.
    ///
    /// Note that only network filters are currently supported by this method.
    pub fn filter_exists(&self, filter: &str) -> bool {
        use crate::filters::network::NetworkFilter;
        let filter_parsed = NetworkFilter::parse(filter, false, Default::default());
        match filter_parsed.map(|f| self.blocker.filter_exists(&f)) {
            Ok(exists) => exists,
            Err(_e) => {
                #[cfg(test)]
                eprintln!("Encountered unparseable filter when checking for filter existence: {:?}", _e);
                false
            }
        }
    }

    /// Sets this engine's tags to be _only_ the ones provided in `tags`.
    ///
    /// Tags can be used to cheaply enable or disable network rules with a corresponding `$tag`
    /// option.
    pub fn use_tags(&mut self, tags: &[&str]) {
        self.blocker.use_tags(tags);
    }

    /// Sets this engine's tags to additionally include the ones provided in `tags`.
    ///
    /// Tags can be used to cheaply enable or disable network rules with a corresponding `$tag`
    /// option.
    pub fn enable_tags(&mut self, tags: &[&str]) {
        self.blocker.enable_tags(tags);
    }

    /// Sets this engine's tags to no longer include the ones provided in `tags`.
    ///
    /// Tags can be used to cheaply enable or disable network rules with a corresponding `$tag`
    /// option.
    pub fn disable_tags(&mut self, tags: &[&str]) {
        self.blocker.disable_tags(tags);
    }

    /// Checks if a given tag exists in this engine.
    ///
    /// Tags can be used to cheaply enable or disable network rules with a corresponding `$tag`
    /// option.
    pub fn tag_exists(&self, tag: &str) -> bool {
        self.blocker.tags_enabled().contains(&tag.to_owned())
    }

    /// Sets this engine's resources to be _only_ the ones provided in `resources`.
    pub fn use_resources(&mut self, resources: impl IntoIterator<Item=Resource>) {
        self.resources = ResourceStorage::from_resources(resources);
    }

    /// Sets this engine's resources to additionally include `resource`.
    pub fn add_resource(&mut self, resource: Resource) -> Result<(), crate::resources::AddResourceError> {
        self.resources.add_resource(resource)
    }

    // Cosmetic filter functionality

    /// If any of the provided CSS classes or ids could cause a certain generic CSS hide rule
    /// (i.e. `{ display: none !important; }`) to be required, this method will return a list of
    /// CSS selectors corresponding to rules referencing those classes or ids, provided that the
    /// corresponding rules are not excepted.
    ///
    /// `exceptions` should be passed directly from `UrlSpecificResources`.
    pub fn hidden_class_id_selectors(&self, classes: impl IntoIterator<Item=impl AsRef<str>>, ids: impl IntoIterator<Item=impl AsRef<str>>, exceptions: &HashSet<String>) -> Vec<String> {
        self.cosmetic_cache.hidden_class_id_selectors(classes, ids, exceptions)
    }

    /// Returns a set of cosmetic filter resources required for a particular url. Once this has
    /// been called, all CSS ids and classes on a page should be passed to
    /// `hidden_class_id_selectors` to obtain any stylesheets consisting of generic rules (if the
    /// returned `generichide` value is false).
    pub fn url_cosmetic_resources(&self, url: &str) -> UrlSpecificResources {
        let request = Request::new(url, url, "document");
        if request.is_err() {
            return UrlSpecificResources::empty();
        }
        let request = request.unwrap();

        let generichide = self.blocker.check_generic_hide(&request);
        self.cosmetic_cache.hostname_cosmetic_resources(&self.resources, &request.hostname, generichide)
    }

    pub fn set_regex_discard_policy(
        &mut self,
        new_discard_policy: RegexManagerDiscardPolicy
    ) {
        self.blocker.set_regex_discard_policy(new_discard_policy);
    }

    #[cfg(feature = "debug-info")]
    pub fn discard_regex(&mut self, regex_id: u64) {
        self.blocker.discard_regex(regex_id);
    }

    #[cfg(feature = "debug-info")]
    pub fn get_debug_info(&self) -> EngineDebugInfo {
        EngineDebugInfo { blocker_debug_info: self.blocker.get_debug_info() }
    }
}

/// Static assertions for `Engine: Send + Sync` traits.
#[cfg(not(any(feature = "object-pooling", feature = "unsync-regex-caching")))]
fn _assertions() {
    fn _assert_send<T: Send>() {}
    fn _assert_sync<T: Sync>() {}

    _assert_send::<Engine>();
    _assert_sync::<Engine>();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resources::MimeType;
    use crate::lists::FilterFormat;

    #[test]
    fn tags_enable_adds_tags() {
        let filters = [
            "adv$tag=stuff",
            "somelongpath/test$tag=stuff",
            "||brianbondy.com/$tag=brian",
            "||brave.com$tag=brian",
        ];
        let url_results = [
            ("http://example.com/advert.html", true),
            ("http://example.com/somelongpath/test/2.html", true),
            ("https://brianbondy.com/about", true),
            ("https://brave.com/about", true),
        ];

        let mut engine = Engine::from_rules(&filters, Default::default());
        engine.enable_tags(&["stuff"]);
        engine.enable_tags(&["brian"]);

        url_results.into_iter().for_each(|(url, expected_result)| {
            let request = Request::new(&url, "", "").unwrap();
            let matched_rule = engine.check_network_request(&request);
            if expected_result {
                assert!(matched_rule.matched, "Expected match for {}", url);
            } else {
                assert!(!matched_rule.matched, "Expected no match for {}, matched with {:?}", url, matched_rule.filter);
            }
        });
    }

    #[test]
    fn tags_disable_works() {
        let filters = [
            "adv$tag=stuff",
            "somelongpath/test$tag=stuff",
            "||brianbondy.com/$tag=brian",
            "||brave.com$tag=brian",
        ];
        let url_results = [
            ("http://example.com/advert.html", false),
            ("http://example.com/somelongpath/test/2.html", false),
            ("https://brianbondy.com/about", true),
            ("https://brave.com/about", true),
        ];

        let mut engine = Engine::from_rules(&filters, Default::default());
        engine.enable_tags(&["brian", "stuff"]);
        engine.disable_tags(&["stuff"]);

        url_results.into_iter().for_each(|(url, expected_result)| {
            let request = Request::new(&url, "", "").unwrap();
            let matched_rule = engine.check_network_request(&request);
            if expected_result {
                assert!(matched_rule.matched, "Expected match for {}", url);
            } else {
                assert!(!matched_rule.matched, "Expected no match for {}, matched with {:?}", url, matched_rule.filter);
            }
        });
    }

    #[test]
    fn exception_tags_inactive_by_default() {
        let filters = [
            "adv",
            "||brianbondy.com/$tag=brian",
            "@@||brianbondy.com/$tag=brian",
        ];
        let url_results = [
            ("http://example.com/advert.html", true),
            ("https://brianbondy.com/about", false),
            ("https://brianbondy.com/advert", true),
        ];

        let engine = Engine::from_rules(&filters, Default::default());

        url_results.into_iter().for_each(|(url, expected_result)| {
            let request = Request::new(&url, "", "").unwrap();
            let matched_rule = engine.check_network_request(&request);
            if expected_result {
                assert!(matched_rule.matched, "Expected match for {}", url);
            } else {
                assert!(!matched_rule.matched, "Expected no match for {}, matched with {:?}", url, matched_rule.filter);
            }
        });
    }

    #[test]
    fn exception_tags_works() {
        let filters = [
            "adv",
            "||brianbondy.com/$tag=brian",
            "@@||brianbondy.com/$tag=brian",
        ];
        let url_results = [
            ("http://example.com/advert.html", true),
            ("https://brianbondy.com/about", false),
            ("https://brianbondy.com/advert", false),
        ];

        let mut engine = Engine::from_rules(&filters, Default::default());
        engine.enable_tags(&["brian", "stuff"]);

        url_results.into_iter().for_each(|(url, expected_result)| {
            let request = Request::new(&url, "", "").unwrap();
            let matched_rule = engine.check_network_request(&request);
            if expected_result {
                assert!(matched_rule.matched, "Expected match for {}", url);
            } else {
                assert!(!matched_rule.matched, "Expected no match for {}, matched with {:?}", url, matched_rule.filter);
            }
        });
    }

    #[test]
    fn serialization_retains_tags() {
        let filters = [
            "adv$tag=stuff",
            "somelongpath/test$tag=stuff",
            "||brianbondy.com/$tag=brian",
            "||brave.com$tag=brian",
        ];
        let url_results = [
            ("http://example.com/advert.html", true),
            ("http://example.com/somelongpath/test/2.html", true),
            ("https://brianbondy.com/about", false),
            ("https://brave.com/about", false),
        ];

        let mut engine = Engine::from_rules(&filters, Default::default());
        engine.enable_tags(&["stuff"]);
        engine.enable_tags(&["brian"]);
        let serialized = engine.serialize_raw().unwrap();
        let mut deserialized_engine = Engine::default();
        deserialized_engine.enable_tags(&["stuff"]);
        deserialized_engine.deserialize(&serialized).unwrap();

        url_results.into_iter().for_each(|(url, expected_result)| {
            let request = Request::new(&url, "", "").unwrap();
            let matched_rule = deserialized_engine.check_network_request(&request);
            if expected_result {
                assert!(matched_rule.matched, "Expected match for {}", url);
            } else {
                assert!(!matched_rule.matched, "Expected no match for {}, matched with {:?}", url, matched_rule.filter);
            }
        });
    }

    #[test]
    fn deserialization_backwards_compatible_plain() {
        // deserialization_generate_simple();
        // assert!(false);
        // converted from the legacy compressed format
        let serialized = [209, 217, 58, 175, 0, 220, 0, 17, 145, 128, 145, 128, 145, 128, 145, 128,
            145, 128, 145, 129, 207, 202, 167, 36, 217, 43, 56, 97, 176, 145, 157, 145, 206, 0, 3,
            31, 255, 129, 1, 169, 97, 100, 45, 98, 97, 110, 110, 101, 114, 192, 192, 192, 192, 192,
            192, 192, 192, 207, 186, 136, 69, 13, 115, 187, 170, 226, 192, 192, 145, 128, 144, 195,
            145, 128, 144, 144, 128, 128, 145, 128, 144, 145, 128];
        let mut deserialized_engine = Engine::default();
        deserialized_engine.deserialize(&serialized).unwrap();

        let url = "http://example.com/ad-banner.gif";
        let request = Request::new(&url, "", "").unwrap();
        let matched_rule = deserialized_engine.check_network_request(&request);
        assert!(matched_rule.matched, "Expected match for {}", url);
    }

    #[test]
    fn deserialization_backwards_compatible_tags() {
        // deserialization_generate_tags();
        // assert!(false);
        // converted from the legacy compressed format
        let serialized = [209, 217, 58, 175, 0, 220, 0, 17, 145, 128, 145, 128, 145, 128, 145, 128,
            145, 128, 145, 128, 145, 128, 145, 157, 145, 206, 0, 3, 31, 255, 129, 1, 169, 97, 100,
            45, 98, 97, 110, 110, 101, 114, 192, 192, 192, 192, 192, 192, 163, 97, 98, 99, 192,
            207, 126, 212, 53, 83, 113, 159, 143, 134, 192, 192, 195, 145, 128, 144, 144, 128, 128,
            145, 128, 144, 145, 128];
        let mut deserialized_engine = Engine::default();

        deserialized_engine.enable_tags(&[]);
        deserialized_engine.deserialize(&serialized).unwrap();
        let url = "http://example.com/ad-banner.gif";
        let request = Request::new(&url, "", "").unwrap();
        let matched_rule = deserialized_engine.check_network_request(&request);
        assert!(!matched_rule.matched, "Expected NO match for {}", url);

        deserialized_engine.enable_tags(&["abc"]);
        deserialized_engine.deserialize(&serialized).unwrap();

        let url = "http://example.com/ad-banner.gif";
        let request = Request::new(&url, "", "").unwrap();
        let matched_rule = deserialized_engine.check_network_request(&request);
        assert!(matched_rule.matched, "Expected match for {}", url);
    }

    #[test]
    fn deserialization_generate_simple() {
        let mut engine = Engine::from_rules(&[
            "ad-banner",
        ], Default::default());
        let serialized = engine.serialize_raw().unwrap();
        println!("Engine serialized: {:?}", serialized);
        engine.deserialize(&serialized).unwrap();
    }

    #[test]
    fn deserialization_generate_tags() {
        let mut engine = Engine::from_rules(&[
            "ad-banner$tag=abc",
        ], Default::default());
        engine.use_tags(&["abc"]);
        let serialized = engine.serialize_raw().unwrap();
        println!("Engine serialized: {:?}", serialized);
        engine.deserialize(&serialized).unwrap();
    }

    #[test]
    fn deserialization_generate_resources() {
        let mut engine = Engine::from_rules(&[
            "ad-banner$redirect=nooptext",
        ], Default::default());

        engine.use_resources([
            Resource::simple("nooptext", MimeType::TextPlain, ""),
            Resource::simple("noopcss", MimeType::TextCss, ""),
        ]);

        let serialized = engine.serialize_raw().unwrap();
        println!("Engine serialized: {:?}", serialized);
        engine.deserialize(&serialized).unwrap();
    }

    #[test]
    fn redirect_resource_insertion_works() {
        let mut engine = Engine::from_rules(&[
            "ad-banner$redirect=nooptext",
            "script.js$redirect=noop.js",
        ], Default::default());

        let script = r#"
(function() {
	;
})();

        "#;
        let mut resources = [
            Resource::simple("nooptext", MimeType::TextPlain, ""),
            Resource::simple("noopjs", MimeType::ApplicationJavascript, script),
        ];
        resources[1].aliases.push("noop.js".to_string());
        engine.use_resources(resources);

        let url = "http://example.com/ad-banner.gif";
        let request = Request::new(url, "", "").unwrap();
        let matched_rule = engine.check_network_request(&request);
        assert!(matched_rule.matched, "Expected match for {}", url);
        assert_eq!(matched_rule.redirect, Some("data:text/plain;base64,".to_owned()), "Expected redirect to contain resource");

        let url = "http://example.com/script.js";
        let request = Request::new(url, "", "").unwrap();
        let matched_rule = engine.check_network_request(&request);
        assert!(matched_rule.matched, "Expected match for {}", url);
        assert_eq!(matched_rule.redirect, Some(format!("data:application/javascript;base64,{}", base64::encode(format!("{}", script)))), "Expected redirect to contain resource");
    }

    #[test]
    fn document() {
        let filters = [
            "||example.com$document",
            "@@||sub.example.com$document",
        ];

        let engine = Engine::from_rules_debug(&filters, Default::default());

        assert!(engine.check_network_request(&Request::new("https://example.com", "https://example.com", "document").unwrap()).matched);
        assert!(!engine.check_network_request(&Request::new("https://example.com", "https://example.com", "script").unwrap()).matched);
        assert!(engine.check_network_request(&Request::new("https://sub.example.com", "https://sub.example.com", "document").unwrap()).exception.is_some());
    }

    #[test]
    fn implicit_all() {
        {
            let engine = Engine::from_rules_debug(["||example.com^"], Default::default());
            assert!(engine.check_network_request(&Request::new("https://example.com", "https://example.com", "document").unwrap()).matched);
        }
        {
            let engine = Engine::from_rules_debug(["||example.com^$first-party"], Default::default());
            assert!(engine.check_network_request(&Request::new("https://example.com", "https://example.com", "document").unwrap()).matched);
        }
        {
            let engine = Engine::from_rules_debug(["||example.com^$script"], Default::default());
            assert!(!engine.check_network_request(&Request::new("https://example.com", "https://example.com", "document").unwrap()).matched);
        }
        {
            let engine = Engine::from_rules_debug(["||example.com^$~script"], Default::default());
            assert!(!engine.check_network_request(&Request::new("https://example.com", "https://example.com", "document").unwrap()).matched);
        }
        {
            let engine = Engine::from_rules_debug(["||example.com^$document", "@@||example.com^$generichide"], Default::default());
            assert!(engine.check_network_request(&Request::new("https://example.com", "https://example.com", "document").unwrap()).matched);
        }
        {
            let engine = Engine::from_rules_debug(["example.com"], ParseOptions { format: FilterFormat::Hosts, ..Default::default() });
            assert!(engine.check_network_request(&Request::new("https://example.com", "https://example.com", "document").unwrap()).matched);
        }
        {
            let engine = Engine::from_rules_debug(["||example.com/path"], Default::default());
            assert!(!engine.check_network_request(&Request::new("https://example.com/path", "https://example.com/path", "document").unwrap()).matched);
        }
        {
            let engine = Engine::from_rules_debug(["||example.com/path^"], Default::default());
            assert!(!engine.check_network_request(&Request::new("https://example.com/path", "https://example.com/path", "document").unwrap()).matched);
        }
    }

    #[test]
    fn generichide() {
        let filters = [
            "##.donotblock",
            "##a[href=\"generic.com\"]",

            "@@||example.com$generichide",
            "example.com##.block",

            "@@||example2.com/test.html$generichide",
            "example2.com##.block",
        ];
        let url_results = [
            ("https://example.com", vec![".block"], true),
            ("https://example.com/test.html", vec![".block"], true),
            ("https://example2.com", vec![".block", "a[href=\"generic.com\"]"], false),
            ("https://example2.com/test.html", vec![".block"], true),
        ];

        let engine = Engine::from_rules(&filters, Default::default());

        url_results.into_iter().for_each(|(url, expected_result, expected_generichide)| {
            let result = engine.url_cosmetic_resources(url);
            assert_eq!(result.hide_selectors, expected_result.iter().map(|s| s.to_string()).collect::<HashSet<_>>());
            assert_eq!(result.generichide, expected_generichide);
        });
    }

    #[test]
    fn important_redirect() {
        let mut filter_set = FilterSet::new(true);
        filter_set.add_filters([
            "||addthis.com^$important,3p,domain=~missingkids.com|~missingkids.org|~sainsburys.jobs|~sitecore.com|~amd.com",
            "||addthis.com/*/addthis_widget.js$script,redirect=addthis.com/addthis_widget.js",
        ], Default::default());
        let mut engine = Engine::from_filter_set(filter_set, false);

        engine.add_resource(
            Resource::simple("addthis.com/addthis_widget.js", MimeType::ApplicationJavascript, "window.addthis = undefined"),
        ).unwrap();

        let request = Request::new("https://s7.addthis.com/js/250/addthis_widget.js?pub=resto", "https://www.rhmodern.com/catalog/product/product.jsp?productId=prod14970086&categoryId=cat7150028", "script").unwrap();
        let result = engine.check_network_request(&request);

        assert!(result.redirect.is_some());
    }

    #[test]
    fn check_match_case_regex_filtering() {
        {
            // match case without regex is discarded
            let engine = Engine::from_rules_debug(["ad.png$match-case"], Default::default());
            let request = Request::new("https://example.com/ad.png", "https://example.com", "image").unwrap();
            assert!(!engine.check_network_request(&request).matched);
        }
        {
            // /^https:\/\/[0-9a-z]{3,}\.[-a-z]{10,}\.(?:li[fv]e|top|xyz)\/[a-z]{8}\/\?utm_campaign=\w{40,}/$doc,match-case,domain=life|live|top|xyz
            let engine = Engine::from_rules_debug([r#"/^https:\/\/[0-9a-z]{3,}\.[-a-z]{10,}\.(?:li[fv]e|top|xyz)\/[a-z]{8}\/\?utm_campaign=\w{40,}/$doc,match-case,domain=life|live|top|xyz"#], Default::default());
            let request = Request::new("https://www.exampleaaa.xyz/testtest/?utm_campaign=aaaaaaaaaabbbbbbbbbbccccccccccdddddddddd", "https://www.exampleaaa.xyz/testtest/?utm_campaign=aaaaaaaaaabbbbbbbbbbccccccccccdddddddddd", "document").unwrap();
            assert!(engine.check_network_request(&request).matched);
        }
        // fails - because of non-supported look around operator in rust regex https://github.com/rust-lang/regex/issues/127#issuecomment-154713666
        /*{
            // /^https?:\/\/((?!www)[a-z]{3,}|\d{2})?\.?[-0-9a-z]{6,}\.[a-z]{2,6}\/(?:[a-z]{6,8}\/)?\/?\?u=[0-9a-z]{7}&o=[0-9a-z]{7}/$doc,frame,match-case,domain=buzz|com|de|fun|guru|info|life|live|mobi|online|pw|site|space|top|us|xyz
            let engine = Engine::from_rules_debug([r#"/^https?:\/\/((?!www)[a-z]{3,}|\d{2})?\.?[-0-9a-z]{6,}\.[a-z]{2,6}\/(?:[a-z]{6,8}\/)?\/?\?u=[0-9a-z]{7}&o=[0-9a-z]{7}/$doc,frame,match-case,domain=buzz|com|de|fun|guru|info|life|live|mobi|online|pw|site|space|top|us|xyz"#], Default::default());
            let request = Request::new("https://example.com/aaaaaa/?u=aaaaaaa&o=bbbbbbb",
                                       "https://example.com/aaaaaa/?u=aaaaaaa&o=bbbbbbb",
                                       "document").unwrap();
            assert!(engine.check_network_request(&request).matched);
        }*/
        // fails - because of non-supported look around operator in rust regex https://github.com/rust-lang/regex/issues/127#issuecomment-154713666
        /*{
            // /^https:\/\/(?:www\d\.)?[-a-z]{6,}\.(?:com|info|net|org)\/(?=[-_a-zA-Z]{0,42}\d)(?=[-_0-9a-z]{0,42}[A-Z])[-_0-9a-zA-Z]{43}\/\?cid=[-_0-9a-zA-Z]{16,36}(?:&qs\d=\S+)?&sid=[_0-9a-f]{1,32}$/$doc,match-case,domain=com|info|net|org
            let engine = Engine::from_rules_debug([r#"/^https:\/\/(?:www\d\.)?[-a-z]{6,}\.(?:com|info|net|org)\/(?=[-_a-zA-Z]{0,42}\d)(?=[-_0-9a-z]{0,42}[A-Z])[-_0-9a-zA-Z]{43}\/\?cid=[-_0-9a-zA-Z]{16,36}(?:&qs\d=\S+)?&sid=[_0-9a-f]{1,32}$/$doc,match-case,domain=com|info|net|org"#], Default::default());
            let request = Request::new("https://www3.example.com/aaaaaaaaaabbbbbbbbbbccccccccccddddddddddAA5/?cid=aaaaaaaaaabbbbbb&qs5=\n&sid=a",
                                       "https://www3.example.com/aaaaaaaaaabbbbbbbbbbccccccccccddddddddddAA5/?cid=aaaaaaaaaabbbbbb&qs5=\n&sid=a",
                                       "document").unwrap();
            assert!(engine.check_network_request(&request).matched);
        }*/
        // fails - because of non-supported look around operator in rust regex https://github.com/rust-lang/regex/issues/127#issuecomment-154713666
        /*{
            // /^https:\/\/(?:www\d\.)?[-a-z]{6,}\.(?:com|info|net|org)\/(?=[-_a-zA-Z]{0,42}\d)(?=[-_0-9a-z]{0,42}[A-Z])[-_0-9a-zA-Z]{43}\/\?sid=[_0-9a-f]{1,32}(?:&qs\d=\S+)?&cid=[-_0-9a-zA-Z]{16,36}$/$doc,match-case,domain=com|info|net|org
            let engine = Engine::from_rules_debug([r#"/^https:\/\/(?:www\d\.)?[-a-z]{6,}\.(?:com|info|net|org)\/(?=[-_a-zA-Z]{0,42}\d)(?=[-_0-9a-z]{0,42}[A-Z])[-_0-9a-zA-Z]{43}\/\?cid=[-_0-9a-zA-Z]{16,36}(?:&qs\d=\S+)?&sid=[_0-9a-f]{1,32}$/$doc,match-case,domain=com|info|net|org"#], Default::default());
            let request = Request::new("https://www3.example.com/aaaaaaaaaabbbbbbbbbbccccccccccddddddddddAA5/?sid=1&qs1=\n&cid=aaaaaaaaaabbbbbb",
                                       "https://www3.example.com/aaaaaaaaaabbbbbbbbbbccccccccccddddddddddAA5/?sid=1&qs1=\n&cid=aaaaaaaaaabbbbbb",
                                       "document").unwrap();
            assert!(engine.check_network_request(&request).matched);
        }*/
        {
            // /^http:\/\/[a-z]{5}\.[a-z]{5}\.com\/[a-z]{10}\.apk$/$doc,match-case,domain=com
            let engine = Engine::from_rules_debug([r#"/^http:\/\/[a-z]{5}\.[a-z]{5}\.com\/[a-z]{10}\.apk$/$doc,match-case,domain=com"#], Default::default());
            let request = Request::new("http://abcde.abcde.com/aaaaabbbbb.apk", "http://abcde.abcde.com/aaaaabbbbb.apk", "document").unwrap();
            assert!(engine.check_network_request(&request).matched);
        }
        // fails - because of non-supported look around operator in rust regex https://github.com/rust-lang/regex/issues/127#issuecomment-154713666
        /*{
            // /\/[A-Z]\/[-0-9a-z]{5,}\.com\/(?:[0-9a-f]{2}\/){3}[0-9a-f]{32}\.js$/$script,1p,match-case
            let engine = Engine::from_rules_debug([r#"/\/[A-Z]\/[-0-9a-z]{5,}\.com\/(?:[0-9a-f]{2}\/){3}[0-9a-f]{32}\.js$/$script,1p,match-case"#], Default::default());
            let request = Request::new("/A/aaaaa.com/aa/bb/cc/aaaaaaaabbbbbbbbccccccccdddddddd.js",
                                       "/A/aaaaa.com/aa/bb/cc/aaaaaaaabbbbbbbbccccccccdddddddd.js",
                                       "script").unwrap();
            assert!(engine.check_network_request(&request).matched);
        }*/
        // fails - because of non-supported look around operator in rust regex https://github.com/rust-lang/regex/issues/127#issuecomment-154713666
        /*{
            // /^https?:\/\/(?:[a-z]{2}\.)?[0-9a-z]{7,16}\.com\/[a-z](?=[a-z]{0,25}[0-9A-Z])[0-9a-zA-Z]{3,26}\/(?:[1-5]\d{4}|[3-9]\d{3})\??(?:_=\d+|v=\d)?$/$frame,script,xhr,popup,3p,match-case
            let engine = Engine::from_rules_debug([r#"/^https?:\/\/(?:[a-z]{2}\.)?[0-9a-z]{7,16}\.com\/[a-z](?=[a-z]{0,25}[0-9A-Z])[0-9a-zA-Z]{3,26}\/(?:[1-5]\d{4}|[3-9]\d{3})\??(?:_=\d+|v=\d)?$/$frame,script,xhr,popup,3p,match-case"#], Default::default());
            let request = Request::new("https://aa.example.com/aAaaa/12222",
                                       "https://aa.example.net/aAaaa/12222",
                                       "frame").unwrap();
            assert!(engine.check_network_request(&request).matched);
        }*/
        // fails - because of non-supported look around operator in rust regex https://github.com/rust-lang/regex/issues/127#issuecomment-154713666
        /*{
            // /^https?:\/\/(?:[a-z]{2}\.)?[0-9a-z]{7,16}\.website\/[a-z](?=[a-z]{0,25}[0-9A-Z])[0-9a-zA-Z]{3,26}\/(?:[1-5]\d{4}|[3-9]\d{3})\??(?:_=\d+|v=\d)?$/$frame,script,xhr,popup,3p,match-case
            let engine = Engine::from_rules_debug([r#"/^https?:\/\/(?:[a-z]{2}\.)?[0-9a-z]{7,16}\.website\/[a-z](?=[a-z]{0,25}[0-9A-Z])[0-9a-zA-Z]{3,26}\/(?:[1-5]\d{4}|[3-9]\d{3})\??(?:_=\d+|v=\d)?$/$frame,script,xhr,popup,3p,match-case"#], Default::default());
            let request = Request::new("https://aa.example.website/aAaaa/12222",
                                       "https://aa.example.website/aAaaa/12222",
                                       "frame").unwrap();
            assert!(engine.check_network_request(&request).matched);
        }*/
        // fails - because of non-supported look around operator in rust regex https://github.com/rust-lang/regex/issues/127#issuecomment-154713666
        /*{
            // /^https?:\/\/[a-z]{8,15}\.top(\/(?:\d{1,5}|0NaN|articles?|browse|index|movie|news|pages?|static|view|web|wiki)){1,4}(?:\.html|\/)$/$frame,3p,match-case
            let engine = Engine::from_rules_debug([r#"/^https?:\/\/[a-z]{8,15}\.top(\/(?:\d{1,5}|0NaN|articles?|browse|index|movie|news|pages?|static|view|web|wiki)){1,4}(?:\.html|\/)$/$frame,3p,match-case"#], Default::default());
            let request = Request::new("https://examples.top/articles.html",
                                       "https://examples.top/articles.html",
                                       "frame").unwrap();
            assert!(engine.check_network_request(&request).matched);
        }*/
        {
            // /^https?:\/\/[a-z]{8,15}\.top\/[a-z]{4,}\.json$/$xhr,3p,match-case
            let engine = Engine::from_rules_debug([r#"/^https?:\/\/[a-z]{8,15}\.top\/[a-z]{4,}\.json$/$xhr,3p,match-case"#], Default::default());
            let request = Request::new("https://examples.top/abcd.json", "https://examples.com/abcd.json", "xhr").unwrap();
            assert!(engine.check_network_request(&request).matched);
        }
        // fails - inferring unescaped `$` inside regex pattern
        /*{
            // /^https?:\/\/[a-z]{8,15}\.top\/[-a-z]{4,}\.css\?aHR0c[\/0-9a-zA-Z]{33,}=?=?$/$css,3p,match-case
            let engine = Engine::from_rules_debug([r#"/^https?:\/\/[a-z]{8,15}\.top\/[-a-z]{4,}\.css\?aHR0c[\/0-9a-zA-Z]{33,}=?=?$/$css,3p,match-case"#], Default::default());
            let request = Request::new("https://examples.top/abcd.css?aHR0c/aaaaaaaaaaAAAAAAAAAA000000000012==",
                                       "https://examples.com/abcd.css?aHR0c/aaaaaaaaaaAAAAAAAAAA000000000012==",
                                       "stylesheet").unwrap();
            assert!(engine.check_network_request(&request).matched);
        }*/
        // fails - inferring unescaped `$` inside regex pattern
        /*{
            // /^https?:\/\/[a-z]{8,15}\.top\/[a-z]{4,}\.png\?aHR0c[\/0-9a-zA-Z]{33,}=?=?$/$image,3p,match-case
            let engine = Engine::from_rules_debug([r#"/^https?:\/\/[a-z]{8,15}\.top\/[a-z]{4,}\.png\?aHR0c[\/0-9a-zA-Z]{33,}=?=?$/$image,3p,match-case"#], Default::default());
            let request = Request::new("https://examples.top/abcd.png?aHR0c/aaaaaaaaaaAAAAAAAAAA000000000012==",
                                       "https://examples.com/abcd.png?aHR0c/aaaaaaaaaaAAAAAAAAAA000000000012==",
                                       "image").unwrap();
            assert!(engine.check_network_request(&request).matched);
        }*/
        // fails - because of non-supported look around operator in rust regex https://github.com/rust-lang/regex/issues/127#issuecomment-154713666
        /*{
            // /^https?:\/\/[a-z]{8,15}\.xyz(\/(?:\d{1,5}|0NaN|articles?|browse|index|movie|news|pages?|static|view|web|wiki)){1,4}(?:\.html|\/)$/$frame,3p,match-case
            let engine = Engine::from_rules_debug([r#"/^https?:\/\/[a-z]{8,15}\.xyz(\/(?:\d{1,5}|0NaN|articles?|browse|index|movie|news|pages?|static|view|web|wiki)){1,4}(?:\.html|\/)$/$frame,3p,match-case"#], Default::default());
            let request = Request::new("https://examples.xyz/articles.html",
                                       "https://examples.xyz/articles.html",
                                       "frame").unwrap();
            assert!(engine.check_network_request(&request).matched);
        }*/
        {
            // /^https?:\/\/cdn\.[a-z]{4,6}\.xyz\/app\.js$/$script,3p,match-case
            let engine = Engine::from_rules_debug([r#"/^https?:\/\/cdn\.[a-z]{4,6}\.xyz\/app\.js$/$script,3p,match-case"#], Default::default());
            let request = Request::new("https://cdn.abcde.xyz/app.js",
                                       "https://cdn.abcde.com/app.js",
                                       "script").unwrap();
            assert!(engine.check_network_request(&request).matched);
        }
        // fails - because of non-supported look around operator in rust regex https://github.com/rust-lang/regex/issues/127#issuecomment-154713666
        /*{
            // /^https:\/\/a\.[-0-9a-z]{4,16}\.(?:club|com?|cyou|info|net|ru|site|top?|xxx|xyz)\/(?=[a-z]{0,6}[0-9A-Z])[0-9a-zA-Z]{7}\.js$/$script,match-case
            let engine = Engine::from_rules_debug([r#"/^https:\/\/a\.[-0-9a-z]{4,16}\.(?:club|com?|cyou|info|net|ru|site|top?|xxx|xyz)\/(?=[a-z]{0,6}[0-9A-Z])[0-9a-zA-Z]{7}\.js$/$script,match-case"#], Default::default());
            let request = Request::new("https://a.abcd.club/aaaaaaA.js",
                                       "https://a.abcd.club/aaaaaaA.js",
                                       "script").unwrap();
            assert!(engine.check_network_request(&request).matched);
        }*/
        {
            // /^https:\/\/cdn\.jsdelivr\.net\/npm\/[-a-z_]{4,22}@latest\/dist\/script\.min\.js$/$script,3p,match-case
            let engine = Engine::from_rules_debug([r#"/^https:\/\/cdn\.jsdelivr\.net\/npm\/[-a-z_]{4,22}@latest\/dist\/script\.min\.js$/$script,3p,match-case"#], Default::default());
            let request = Request::new("https://cdn.jsdelivr.net/npm/abcd@latest/dist/script.min.js",
                                       "https://cdn.jsdelivr.com/npm/abcd@latest/dist/script.min.js",
                                       "script").unwrap();
            assert!(engine.check_network_request(&request).matched);
        }
        // fails - inferring unescaped `$` inside regex pattern
        /*{
            // /^https?:\/\/[-.0-9a-z]+\/script\.js$/$script,1p,strict3p,match-case
            let engine = Engine::from_rules_debug([r#"/^https?:\/\/[-.0-9a-z]+\/script\.js$/$script,1p,strict3p,match-case"#], Default::default());
            let request = Request::new("https://www.example.com/script.js",
                                       "https://www.abc.com/script.js",
                                       "script").unwrap();
            assert!(engine.check_network_request(&request).matched);
        }*/
        {
            let engine = Engine::from_rules_debug([r#"/tesT߶/$domain=example.com"#], Default::default());
            let request = Request::new("https://example.com/tesT߶",
                                       "https://example.com",
                                       "script").unwrap();
            assert!(engine.check_network_request(&request).matched);
        }
        // fails - punycoded domain
        /*{
            let engine = Engine::from_rules_debug([r#"/tesT߶/$domain=example.com"#], Default::default());
            let request = Request::new("https://example-tesT߶.com/tesT",
                                       "https://example.com",
                                       "script").unwrap();
            assert!(engine.check_network_request(&request).matched);
        }*/
    }

    #[test]
    fn scriptlet_permissions() {
        use crate::resources::{PermissionMask, ResourceType};
        const UBO_PERM: PermissionMask = PermissionMask::from_bits(0b00000001);
        const BRAVE_PERM: PermissionMask = PermissionMask::from_bits(0b00000011);

        let resources = [
            Resource::simple("refresh-defuser.js", MimeType::ApplicationJavascript, "refresh-defuser"),
            Resource {
                name: "trusted-set-cookie.js".to_string(),
                aliases: vec![],
                kind: ResourceType::Mime(MimeType::ApplicationJavascript),
                content: base64::encode("trusted-set-cookie"),
                dependencies: vec![],
                permission: UBO_PERM,
            },
            Resource {
                name: "brave-fix.js".to_string(),
                aliases: vec![],
                kind: ResourceType::Mime(MimeType::ApplicationJavascript),
                content: base64::encode("brave-fix"),
                dependencies: vec![],
                permission: BRAVE_PERM,
            },
        ];

        let mut filter_set = FilterSet::new(false);
        filter_set.add_filters([
            "sub1.example.com##+js(refresh-defuser)",
            "sub2.example.com##+js(trusted-set-cookie)",
            "sub3.example.com##+js(brave-fix)"
        ], Default::default());
        filter_set.add_filters([
            "sub4.example.com##+js(refresh-defuser)",
            "sub5.example.com##+js(trusted-set-cookie)",
            "sub6.example.com##+js(brave-fix)"
        ], ParseOptions {
            permissions: UBO_PERM,
            ..Default::default()
        });
        filter_set.add_filters([
            "sub7.example.com##+js(refresh-defuser)",
            "sub8.example.com##+js(trusted-set-cookie)",
            "sub9.example.com##+js(brave-fix)"
        ], ParseOptions {
            permissions: BRAVE_PERM,
            ..Default::default()
        });

        let mut engine = Engine::from_filter_set(filter_set, true);
        engine.use_resources(resources);

        fn wrap_try(scriptlet_content: &str) -> String {
            format!("try {{\n{}\n}} catch ( e ) {{ }}\n", scriptlet_content)
        }

        assert_eq!(engine.url_cosmetic_resources("https://sub1.example.com").injected_script, wrap_try("refresh-defuser"));
        assert_eq!(engine.url_cosmetic_resources("https://sub2.example.com").injected_script, "");
        assert_eq!(engine.url_cosmetic_resources("https://sub3.example.com").injected_script, "");

        assert_eq!(engine.url_cosmetic_resources("https://sub4.example.com").injected_script, wrap_try("refresh-defuser"));
        assert_eq!(engine.url_cosmetic_resources("https://sub5.example.com").injected_script, wrap_try("trusted-set-cookie"));
        assert_eq!(engine.url_cosmetic_resources("https://sub6.example.com").injected_script, "");

        assert_eq!(engine.url_cosmetic_resources("https://sub7.example.com").injected_script, wrap_try("refresh-defuser"));
        assert_eq!(engine.url_cosmetic_resources("https://sub8.example.com").injected_script, wrap_try("trusted-set-cookie"));
        assert_eq!(engine.url_cosmetic_resources("https://sub9.example.com").injected_script, wrap_try("brave-fix"));
    }
}
