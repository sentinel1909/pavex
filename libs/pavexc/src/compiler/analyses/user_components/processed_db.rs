use ahash::HashMap;
use guppy::graph::PackageGraph;
use indexmap::IndexSet;
use std::collections::BTreeMap;
use syn::spanned::Spanned;

use pavex_bp_schema::{
    Blueprint, CloningStrategy, Lifecycle, Lint, LintSetting, Location, RawIdentifiers,
};
use pavex_cli_diagnostic::anyhow2miette;

use crate::compiler::analyses::computations::ComputationDb;
use crate::compiler::analyses::config_types::ConfigTypeDb;
use crate::compiler::analyses::prebuilt_types::PrebuiltTypeDb;
use crate::compiler::analyses::user_components::raw_db::RawUserComponentDb;
use crate::compiler::analyses::user_components::resolved_paths::ResolvedPathDb;
use crate::compiler::analyses::user_components::router::Router;
use crate::compiler::analyses::user_components::{ScopeGraph, UserComponent, UserComponentId};
use crate::compiler::component::{
    ConfigType, ConfigTypeValidationError, DefaultStrategy, PrebuiltType,
    PrebuiltTypeValidationError,
};
use crate::compiler::interner::Interner;
use crate::compiler::resolvers::{CallableResolutionError, TypeResolutionError, resolve_type_path};
use crate::diagnostic::{
    AnnotatedSnippet, CallableDefinition, CompilerDiagnostic, OptionalSourceSpanExt, SourceSpanExt,
};
use crate::language::ResolvedPath;
use crate::rustdoc::CrateCollection;
use crate::utils::comma_separated_list;
use crate::{diagnostic, try_source};

/// A database that contains all the user components that have been registered against the
/// `Blueprint` for the application.
///
/// For each component, we keep track of:
/// - the source code location where it was registered (for error reporting purposes);
/// - the lifecycle of the component;
/// - the scope that the component belongs to.
///
/// Some basic validation has been carried out:
/// - the callable associated to each component has been resolved and added to the
///   provided [`ComputationDb`].
/// - there are no conflicting routes.
#[derive(Debug)]
pub struct UserComponentDb {
    component_interner: Interner<UserComponent>,
    identifiers_interner: Interner<RawIdentifiers>,
    /// Associate each user-registered component with the location it was
    /// registered at against the `Blueprint` in the user's source code.
    ///
    /// Invariants: there is an entry for every single user component.
    id2locations: HashMap<UserComponentId, Location>,
    /// Associate each user-registered component with its lifecycle.
    ///
    /// Invariants: there is an entry for every single user component.
    id2lifecycle: HashMap<UserComponentId, Lifecycle>,
    /// Associate each user-registered component with its lint overrides, if any.
    /// If there is no entry for a component, there are no overrides.
    id2lints: HashMap<UserComponentId, BTreeMap<Lint, LintSetting>>,
    /// For each constructible component, determine if it can be cloned or not.
    ///
    /// Invariants: there is an entry for every constructor and prebuilt type.
    id2cloning_strategy: HashMap<UserComponentId, CloningStrategy>,
    /// Determine if a configuration type should have a default.
    ///
    /// Invariants: there is an entry for configuration type.
    config_id2default_strategy: HashMap<UserComponentId, DefaultStrategy>,
    /// Associate each request handler with the ordered list of middlewares that wrap around it.
    ///
    /// Invariants: there is an entry for every single request handler.
    handler_id2middleware_ids: HashMap<UserComponentId, Vec<UserComponentId>>,
    /// Associate each request handler with the ordered list of error observers
    /// that must be invoked when an error occurs while handling a request.
    ///
    /// Invariants: there is an entry for every single request handler.
    handler_id2error_observer_ids: HashMap<UserComponentId, Vec<UserComponentId>>,
    scope_graph: ScopeGraph,
}

impl UserComponentDb {
    /// Process a `Blueprint` and return a `UserComponentDb` that contains all the user components
    /// that have been registered against it.
    ///
    /// The callable associated to each component will be resolved and added to the
    /// provided [`ComputationDb`].
    #[tracing::instrument(name = "Build user component database", skip_all)]
    pub(crate) fn build(
        bp: &Blueprint,
        computation_db: &mut ComputationDb,
        prebuilt_type_db: &mut PrebuiltTypeDb,
        config_type_db: &mut ConfigTypeDb,
        package_graph: &PackageGraph,
        krate_collection: &CrateCollection,
        diagnostics: &mut Vec<miette::Error>,
    ) -> Result<(Router, Self), ()> {
        /// Exit early if there is at least one error.
        macro_rules! exit_on_errors {
            ($var:ident) => {
                if !$var.is_empty() {
                    return Err(());
                }
            };
        }

        let (raw_db, scope_graph) = RawUserComponentDb::build(bp, package_graph, diagnostics);
        let resolved_path_db = ResolvedPathDb::build(&raw_db, package_graph, diagnostics);
        let router = Router::new(&raw_db, &scope_graph, package_graph, diagnostics)?;
        exit_on_errors!(diagnostics);

        precompute_crate_docs(krate_collection, &resolved_path_db, diagnostics);
        exit_on_errors!(diagnostics);

        Self::resolve_and_intern_paths(
            &resolved_path_db,
            &raw_db,
            computation_db,
            prebuilt_type_db,
            config_type_db,
            package_graph,
            krate_collection,
            diagnostics,
        );
        exit_on_errors!(diagnostics);

        let RawUserComponentDb {
            component_interner,
            id2locations,
            id2lints,
            id2cloning_strategy,
            id2lifecycle,
            identifiers_interner,
            config_id2default_strategy,
            handler_id2middleware_ids,
            handler_id2error_observer_ids,
            fallback_id2domain_guard: _,
            fallback_id2path_prefix: _,
            domain_guard2locations: _,
        } = raw_db;

        Ok((
            router,
            Self {
                component_interner,
                identifiers_interner,
                id2locations,
                id2cloning_strategy,
                id2lifecycle,
                config_id2default_strategy,
                handler_id2middleware_ids,
                handler_id2error_observer_ids,
                scope_graph,
                id2lints,
            },
        ))
    }

    /// Iterate over all the user components in the database, returning their id and the associated
    /// `UserComponent`.
    pub fn iter(
        &self,
    ) -> impl ExactSizeIterator<Item = (UserComponentId, &UserComponent)> + DoubleEndedIterator
    {
        self.component_interner.iter()
    }

    /// Iterate over all the constructor components in the database, returning their id and the
    /// associated `UserComponent`.
    pub fn constructors(
        &self,
    ) -> impl DoubleEndedIterator<Item = (UserComponentId, &UserComponent)> {
        self.component_interner
            .iter()
            .filter(|(_, c)| matches!(c, UserComponent::Constructor { .. }))
    }

    /// Iterate over all the prebuilt types in the database, returning their id and the
    /// associated `UserComponent`.
    pub fn prebuilt_types(
        &self,
    ) -> impl DoubleEndedIterator<Item = (UserComponentId, &UserComponent)> {
        self.component_interner
            .iter()
            .filter(|(_, c)| matches!(c, UserComponent::PrebuiltType { .. }))
    }

    /// Iterate over all the config types in the database, returning their id and the
    /// associated `UserComponent`.
    pub fn config_types(
        &self,
    ) -> impl DoubleEndedIterator<Item = (UserComponentId, &UserComponent)> {
        self.component_interner
            .iter()
            .filter(|(_, c)| matches!(c, UserComponent::ConfigType { .. }))
    }

    /// Iterate over all the request handler components in the database, returning their id and the
    /// associated `UserComponent`.
    ///
    /// It returns both routes (i.e. handlers that are registered against a specific path and method
    /// guard) and fallback handlers.
    pub fn request_handlers(
        &self,
    ) -> impl DoubleEndedIterator<Item = (UserComponentId, &UserComponent)> {
        self.component_interner.iter().filter(|(_, c)| {
            matches!(
                c,
                UserComponent::RequestHandler { .. } | UserComponent::Fallback { .. }
            )
        })
    }

    /// Iterate over all the wrapping middleware components in the database, returning their id and the
    /// associated `UserComponent`.
    pub fn wrapping_middlewares(
        &self,
    ) -> impl DoubleEndedIterator<Item = (UserComponentId, &UserComponent)> {
        self.component_interner
            .iter()
            .filter(|(_, c)| matches!(c, UserComponent::WrappingMiddleware { .. }))
    }

    /// Iterate over all the post-processing middleware components in the database, returning their id and the
    /// associated `UserComponent`.
    pub fn post_processing_middlewares(
        &self,
    ) -> impl DoubleEndedIterator<Item = (UserComponentId, &UserComponent)> {
        self.component_interner
            .iter()
            .filter(|(_, c)| matches!(c, UserComponent::PostProcessingMiddleware { .. }))
    }

    /// Iterate over all the pre-processing middleware components in the database, returning their id and the
    /// associated `UserComponent`.
    pub fn pre_processing_middlewares(
        &self,
    ) -> impl DoubleEndedIterator<Item = (UserComponentId, &UserComponent)> {
        self.component_interner
            .iter()
            .filter(|(_, c)| matches!(c, UserComponent::PreProcessingMiddleware { .. }))
    }

    /// Iterate over all the error observer components in the database, returning their id and the
    /// associated `UserComponent`.
    pub fn error_observers(
        &self,
    ) -> impl DoubleEndedIterator<Item = (UserComponentId, &UserComponent)> {
        self.component_interner
            .iter()
            .filter(|(_, c)| matches!(c, UserComponent::ErrorObserver { .. }))
    }

    /// Return the lifecycle of the component with the given id.
    pub fn get_lifecycle(&self, id: UserComponentId) -> Lifecycle {
        self.id2lifecycle[&id]
    }

    /// Return the location where the component with the given id was registered against the
    /// application blueprint.
    pub fn get_location(&self, id: UserComponentId) -> &Location {
        &self.id2locations[&id]
    }

    /// Return the cloning strategy of the component with the given id.
    /// This is going to be `Some(..)` for constructor and prebuilt type components,
    /// and `None` for all other components.
    pub fn get_cloning_strategy(&self, id: UserComponentId) -> Option<&CloningStrategy> {
        self.id2cloning_strategy.get(&id)
    }

    /// Return the default strategy of the configuration component with the given id.
    /// This is going to be `Some(..)` for configuration components,
    /// and `None` for all other components.
    pub fn get_default_strategy(&self, id: UserComponentId) -> Option<&DefaultStrategy> {
        self.config_id2default_strategy.get(&id)
    }

    /// Return the scope tree that was built from the application blueprint.
    pub fn scope_graph(&self) -> &ScopeGraph {
        &self.scope_graph
    }

    /// Return the raw callable identifiers associated to the user component with the given id.
    ///
    /// This can be used to recover the original import path passed by the user when registering
    /// this component, primarily for error reporting purposes.
    pub fn get_raw_callable_identifiers(&self, id: UserComponentId) -> &RawIdentifiers {
        let raw_id = self.component_interner[id].raw_identifiers_id();
        &self.identifiers_interner[raw_id]
    }

    /// Return the ids of the middlewares that wrap around the request handler with the given id.
    ///
    /// It panics if the component with the given id is not a request handler.
    pub fn get_middleware_ids(&self, id: UserComponentId) -> &[UserComponentId] {
        &self.handler_id2middleware_ids[&id]
    }

    /// Return the lint overrides for this component, if any.
    pub fn get_lints(&self, id: UserComponentId) -> Option<&BTreeMap<Lint, LintSetting>> {
        self.id2lints.get(&id)
    }

    /// Return the ids of the error observers that must be invoked when something goes wrong
    /// in the request processing pipeline for this handler.
    ///
    /// It panics if the component with the given id is not a request handler.
    pub fn get_error_observer_ids(&self, id: UserComponentId) -> &[UserComponentId] {
        &self.handler_id2error_observer_ids[&id]
    }
}

/// We try to batch together the computation of the JSON documentation for all the crates that,
/// based on the information we have so far, will be needed to generate the application code.
///
/// This is not strictly necessary, but it can turn out to be a significant performance improvement
/// for projects that pull in a lot of dependencies in the signature of their components.
fn precompute_crate_docs(
    krate_collection: &CrateCollection,
    resolved_path_db: &ResolvedPathDb,
    diagnostics: &mut Vec<miette::Error>,
) {
    let mut package_ids = IndexSet::new();
    for (_, path) in resolved_path_db.iter() {
        path.collect_package_ids(&mut package_ids);
    }
    if let Err(e) = krate_collection.bootstrap_collection(package_ids.into_iter().cloned()) {
        let e = anyhow::anyhow!(e).context(
            "I failed to compute the JSON documentation for one or more crates in the workspace.",
        );
        diagnostics.push(anyhow2miette(e));
    }
}

impl UserComponentDb {
    /// Resolve and intern all the paths in the `ResolvedPathDb`.
    /// Report errors as diagnostics if any of the paths cannot be resolved.
    #[tracing::instrument(name = "Resolve and intern paths", skip_all, level = "trace")]
    fn resolve_and_intern_paths(
        resolved_path_db: &ResolvedPathDb,
        raw_db: &RawUserComponentDb,
        computation_db: &mut ComputationDb,
        prebuilt_type_db: &mut PrebuiltTypeDb,
        config_type_db: &mut ConfigTypeDb,
        package_graph: &PackageGraph,
        krate_collection: &CrateCollection,
        diagnostics: &mut Vec<miette::Error>,
    ) {
        for (raw_id, user_component) in raw_db.iter() {
            let resolved_path = &resolved_path_db[raw_id];
            if let UserComponent::PrebuiltType { .. } = &user_component {
                match resolve_type_path(resolved_path, krate_collection) {
                    Ok(ty) => match PrebuiltType::new(ty) {
                        Ok(prebuilt) => {
                            prebuilt_type_db.get_or_intern(prebuilt, raw_id);
                        }
                        Err(e) => Self::invalid_prebuilt_type(
                            e,
                            resolved_path,
                            raw_id,
                            raw_db,
                            package_graph,
                            diagnostics,
                        ),
                    },
                    Err(e) => Self::cannot_resolve_type_path(
                        e,
                        raw_id,
                        raw_db,
                        package_graph,
                        diagnostics,
                    ),
                };
            } else if let UserComponent::ConfigType { key, .. } = &user_component {
                match resolve_type_path(resolved_path, krate_collection) {
                    Ok(ty) => match ConfigType::new(ty, key.into()) {
                        Ok(config) => {
                            config_type_db.get_or_intern(config, raw_id);
                        }
                        Err(e) => Self::invalid_config_type(
                            e,
                            resolved_path,
                            raw_id,
                            raw_db,
                            package_graph,
                            diagnostics,
                        ),
                    },
                    Err(e) => Self::cannot_resolve_type_path(
                        e,
                        raw_id,
                        raw_db,
                        package_graph,
                        diagnostics,
                    ),
                };
            } else if let Err(e) =
                computation_db.resolve_and_intern(krate_collection, resolved_path, Some(raw_id))
            {
                Self::cannot_resolve_callable_path(e, raw_id, raw_db, package_graph, diagnostics);
            }
        }
    }

    fn invalid_prebuilt_type(
        e: PrebuiltTypeValidationError,
        resolved_path: &ResolvedPath,
        component_id: UserComponentId,
        component_db: &RawUserComponentDb,
        package_graph: &PackageGraph,
        diagnostics: &mut Vec<miette::Error>,
    ) {
        use std::fmt::Write as _;

        let location = component_db.get_location(component_id);
        let source = try_source!(location, package_graph, diagnostics);
        let label = source.as_ref().and_then(|source| {
            diagnostic::get_f_macro_invocation_span(source, location)
                .labeled("The prebuilt type was registered here".to_string())
        });
        let mut error_msg = e.to_string();
        let help: String = match &e {
            PrebuiltTypeValidationError::CannotHaveLifetimeParameters { ty } => {
                if ty.has_implicit_lifetime_parameters() {
                    writeln!(
                        &mut error_msg,
                        "\n`{resolved_path}` has elided lifetime parameters, which might be non-'static."
                    ).unwrap();
                } else {
                    let named_lifetimes = ty.named_lifetime_parameters();
                    if named_lifetimes.len() == 1 {
                        write!(
                            &mut error_msg,
                            "\n`{resolved_path}` has a named lifetime parameter, `'{}`, that you haven't constrained to be 'static.",
                            named_lifetimes[0]
                        ).unwrap();
                    } else {
                        write!(
                            &mut error_msg,
                            "\n`{resolved_path}` has {} named lifetime parameters that you haven't constrained to be 'static: ",
                            named_lifetimes.len(),
                        ).unwrap();
                        comma_separated_list(
                            &mut error_msg,
                            named_lifetimes.iter(),
                            |s| format!("`'{s}`"),
                            "and",
                        )
                        .unwrap();
                        write!(&mut error_msg, ".").unwrap();
                    }
                };
                "Set the lifetime parameters to `'static` when registering the type as prebuilt. E.g. `bp.prebuilt(t!(crate::MyType<'static>))` for `struct MyType<'a>(&'a str)`.".to_string()
            }
            PrebuiltTypeValidationError::CannotHaveUnassignedGenericTypeParameters { ty } => {
                let generic_type_parameters = ty.unassigned_generic_type_parameters();
                if generic_type_parameters.len() == 1 {
                    write!(
                        &mut error_msg,
                        "\n`{resolved_path}` has a generic type parameter, `{}`, that you haven't assigned a concrete type to.",
                        generic_type_parameters[0]
                    ).unwrap();
                } else {
                    write!(
                        &mut error_msg,
                        "\n`{resolved_path}` has {} generic type parameters that you haven't assigned concrete types to: ",
                        generic_type_parameters.len(),
                    ).unwrap();
                    comma_separated_list(
                        &mut error_msg,
                        generic_type_parameters.iter(),
                        |s| format!("`{s}`"),
                        "and",
                    )
                    .unwrap();
                    write!(&mut error_msg, ".").unwrap();
                }
                "Set the generic parameters to concrete types when registering the type as prebuilt. E.g. `bp.prebuilt(t!(crate::MyType<std::string::String>))` for `struct MyType<T>(T)`.".to_string()
            }
        };
        let e = anyhow::anyhow!(e).context(error_msg);
        let diagnostic = CompilerDiagnostic::builder(e)
            .optional_source(source)
            .optional_label(label)
            .help(help)
            .build();
        diagnostics.push(diagnostic.into());
    }

    fn invalid_config_type(
        e: ConfigTypeValidationError,
        resolved_path: &ResolvedPath,
        component_id: UserComponentId,
        component_db: &RawUserComponentDb,
        package_graph: &PackageGraph,
        diagnostics: &mut Vec<miette::Error>,
    ) {
        use std::fmt::Write as _;

        let location = component_db.get_location(component_id);
        let source = try_source!(location, package_graph, diagnostics);
        let label = source.as_ref().and_then(|source| {
            use ConfigTypeValidationError::*;

            match &e {
                CannotHaveAnyLifetimeParameters { .. }
                | CannotHaveUnassignedGenericTypeParameters { .. } => {
                    diagnostic::get_f_macro_invocation_span(source, location)
                        .labeled("The config type was registered here".to_string())
                }
                InvalidKey { .. } => diagnostic::get_config_key_span(source, location)
                    .labeled("The config key was registered here".to_string()),
            }
        });
        let mut error_msg = e.to_string();
        let help: Option<String> = match &e {
            ConfigTypeValidationError::InvalidKey { .. } => None,
            ConfigTypeValidationError::CannotHaveAnyLifetimeParameters { ty } => {
                let named = ty.named_lifetime_parameters();
                let (elided, static_) = ty
                    .lifetime_parameters()
                    .into_iter()
                    .filter(|l| l.is_static() || l.is_elided())
                    .partition::<Vec<_>, _>(|l| l.is_elided());
                write!(&mut error_msg, "\n`{resolved_path}` has ").unwrap();
                let mut parts = Vec::new();
                if !named.is_empty() {
                    let n = named.len();
                    let part = if n == 1 {
                        let lifetime = &named[0];
                        format!("1 named lifetime parameter (`'{lifetime}'`)")
                    } else {
                        let mut part = String::new();
                        write!(&mut part, "{n} named lifetime parameters (").unwrap();
                        comma_separated_list(&mut part, named.iter(), |s| format!("`'{s}`"), "and")
                            .unwrap();
                        part.push(')');
                        part
                    };
                    parts.push(part);
                }
                if !elided.is_empty() {
                    let n = elided.len();
                    let part = if n == 1 {
                        "1 elided lifetime parameter".to_string()
                    } else {
                        format!("{n} elided lifetime parameters")
                    };
                    parts.push(part);
                }
                if !static_.is_empty() {
                    let n = static_.len();
                    let part = if n == 1 {
                        "1 static lifetime parameter".to_string()
                    } else {
                        format!("{n} static lifetime parameters")
                    };
                    parts.push(part);
                }
                comma_separated_list(&mut error_msg, parts.iter(), |s| s.to_owned(), "and")
                    .unwrap();
                error_msg.push('.');

                Some("Remove all lifetime parameters from the definition of your configuration type."
                    .to_string())
            }
            ConfigTypeValidationError::CannotHaveUnassignedGenericTypeParameters { ty } => {
                let generic_type_parameters = ty.unassigned_generic_type_parameters();
                if generic_type_parameters.len() == 1 {
                    write!(
                            &mut error_msg,
                            "\n`{resolved_path}` has a generic type parameter, `{}`, that you haven't assigned a concrete type to.",
                            generic_type_parameters[0]
                        ).unwrap();
                } else {
                    write!(
                            &mut error_msg,
                            "\n`{resolved_path}` has {} generic type parameters that you haven't assigned concrete types to: ",
                            generic_type_parameters.len(),
                        ).unwrap();
                    comma_separated_list(
                        &mut error_msg,
                        generic_type_parameters.iter(),
                        |s| format!("`{s}`"),
                        "and",
                    )
                    .unwrap();
                    write!(&mut error_msg, ".").unwrap();
                }
                Some("Set the generic parameters to concrete types when registering the type as configuration. E.g. `bp.config(t!(crate::MyType<std::string::String>))` for `struct MyType<T>(T)`.".to_string())
            }
        };
        let e = anyhow::anyhow!(e).context(error_msg);
        let diagnostic = CompilerDiagnostic::builder(e)
            .optional_source(source)
            .optional_label(label)
            .optional_help(help)
            .build();
        diagnostics.push(diagnostic.into());
    }

    fn cannot_resolve_type_path(
        e: TypeResolutionError,
        component_id: UserComponentId,
        component_db: &RawUserComponentDb,
        package_graph: &PackageGraph,
        diagnostics: &mut Vec<miette::Error>,
    ) {
        let location = component_db.get_location(component_id);
        let source = try_source!(location, package_graph, diagnostics);
        let label = source.as_ref().and_then(|source| {
            diagnostic::get_f_macro_invocation_span(source, location)
                .labeled("The type that we can't resolve".to_string())
        });
        let diagnostic = CompilerDiagnostic::builder(e)
            .optional_source(source)
            .optional_label(label)
            .help("Check that the path is spelled correctly and that the type is public.".into())
            .build();
        diagnostics.push(diagnostic.into());
    }

    fn cannot_resolve_callable_path(
        e: CallableResolutionError,
        component_id: UserComponentId,
        component_db: &RawUserComponentDb,
        package_graph: &PackageGraph,
        diagnostics: &mut Vec<miette::Error>,
    ) {
        let location = component_db.get_location(component_id);
        let component = &component_db[component_id];
        let callable_type = component.kind();
        let source = try_source!(location, package_graph, diagnostics);
        match e {
            CallableResolutionError::UnknownCallable(_) => {
                let label = source.as_ref().and_then(|source| {
                    diagnostic::get_f_macro_invocation_span(source, location)
                        .labeled(format!("The {callable_type} that we can't resolve"))
                });
                let diagnostic = CompilerDiagnostic::builder(e).optional_source(source)
                    .optional_label(label)
                    .help("Check that the path is spelled correctly and that the function (or method) is public.".into())
                    .build();
                diagnostics.push(diagnostic.into());
            }
            CallableResolutionError::InputParameterResolutionError(ref inner_error) => {
                let definition_snippet = match CallableDefinition::compute_from_item(
                    &inner_error.callable_item,
                    package_graph,
                ) {
                    Some(def) => {
                        let mut inputs = def.sig.inputs.iter();
                        let input = inputs.nth(inner_error.parameter_index).cloned().unwrap();
                        let local_span = match input {
                            syn::FnArg::Typed(typed) => typed.ty.span(),
                            syn::FnArg::Receiver(r) => r.span(),
                        };
                        let label = def
                            .convert_local_span(local_span)
                            .labeled("I don't know how handle this parameter".into());
                        Some(AnnotatedSnippet::new(def.named_source(), label))
                    }
                    _ => None,
                };
                let label = source.as_ref().and_then(|source| {
                    diagnostic::get_f_macro_invocation_span(source, location)
                        .labeled(format!("The {callable_type} was registered here"))
                });
                let diagnostic = CompilerDiagnostic::builder(e.clone())
                    .optional_source(source)
                    .optional_label(label)
                    .optional_additional_annotated_snippet(definition_snippet)
                    .build();
                diagnostics.push(diagnostic.into());
            }
            CallableResolutionError::UnsupportedCallableKind(ref inner_error) => {
                let label = source.as_ref().and_then(|source| {
                    diagnostic::get_f_macro_invocation_span(source, location)
                        .labeled(format!("It was registered as a {callable_type} here"))
                });
                let message = format!(
                    "I can work with functions and methods, but `{}` is neither.\nIt is {} and I don't know how to use it as a {}.",
                    inner_error.import_path, inner_error.item_kind, callable_type
                );
                let error = anyhow::anyhow!(e).context(message);
                diagnostics.push(
                    CompilerDiagnostic::builder(error)
                        .optional_source(source)
                        .optional_label(label)
                        .build()
                        .into(),
                );
            }
            CallableResolutionError::OutputTypeResolutionError(ref inner_error) => {
                let annotated_snippet = {
                    match CallableDefinition::compute_from_item(
                        &inner_error.callable_item,
                        package_graph,
                    ) {
                        Some(def) => match &def.sig.output {
                            syn::ReturnType::Default => None,
                            syn::ReturnType::Type(_, type_) => Some(type_.span()),
                        }
                        .map(|s| {
                            let label = def
                                .convert_local_span(s)
                                .labeled("The output type that I can't handle".into());
                            AnnotatedSnippet::new(def.named_source(), label)
                        }),
                        _ => None,
                    }
                };

                let label = source.as_ref().and_then(|source| {
                    diagnostic::get_f_macro_invocation_span(source, location)
                        .labeled(format!("The {callable_type} was registered here"))
                });
                diagnostics.push(
                    CompilerDiagnostic::builder(e.clone())
                        .optional_source(source)
                        .optional_label(label)
                        .optional_additional_annotated_snippet(annotated_snippet)
                        .build()
                        .into(),
                )
            }
            CallableResolutionError::CannotGetCrateData(_) => {
                diagnostics.push(CompilerDiagnostic::builder(e).build().into());
            }
            CallableResolutionError::GenericParameterResolutionError(_) => {
                let label = source.as_ref().and_then(|source| {
                    diagnostic::get_f_macro_invocation_span(source, location)
                        .labeled(format!("The {callable_type} was registered here"))
                });
                let diagnostic = CompilerDiagnostic::builder(e)
                    .optional_source(source)
                    .optional_label(label)
                    .build();
                diagnostics.push(diagnostic.into());
            }
        }
    }
}

impl std::ops::Index<UserComponentId> for UserComponentDb {
    type Output = UserComponent;

    fn index(&self, index: UserComponentId) -> &Self::Output {
        &self.component_interner[index]
    }
}

impl std::ops::Index<&UserComponent> for UserComponentDb {
    type Output = UserComponentId;

    fn index(&self, index: &UserComponent) -> &Self::Output {
        &self.component_interner[index]
    }
}
