//! The interface implemented by connectors for Prisma schema validation and interpretation.

/// Connector capabilities
pub mod capabilities;
/// Constraint name defaults.
pub mod constraint_names;
/// Extensions for parser database walkers with context from the connector.
pub mod walker_ext_traits;

/// Connector completions
pub mod completions;

mod empty_connector;
mod filters;
mod native_types;
mod relation_mode;

pub use self::{
    capabilities::{ConnectorCapabilities, ConnectorCapability},
    completions::format_completion_docs,
    empty_connector::EmptyDatamodelConnector,
    filters::*,
    native_types::{NativeTypeArguments, NativeTypeConstructor, NativeTypeInstance},
    relation_mode::RelationMode,
};

use crate::{configuration::DatasourceConnectorData, Configuration, Datasource, PreviewFeature};
use chrono::{DateTime, FixedOffset};
use diagnostics::{DatamodelError, Diagnostics, NativeTypeErrorFactory, Span};
use enumflags2::BitFlags;
use lsp_types::CompletionList;
use parser_database::{
    ast::{self, SchemaPosition},
    walkers, IndexAlgorithm, ParserDatabase, ReferentialAction, ScalarType,
};
use std::{borrow::Cow, collections::HashMap, str::FromStr};

pub const EXTENSIONS_KEY: &str = "extensions";

pub trait ValidatedConnector: Send + Sync {
    /// The name of the provider, for string comparisons determining which connector we are on.
    fn provider_name(&self) -> &'static str;

    /// Must return true whenever the passed in provider name is a match.
    fn is_provider(&self, name: &str) -> bool {
        name == self.provider_name()
    }

    /// The name of the connector. Can be used in error messages.
    fn name(&self) -> &str;

    /// The static list of capabilities for the connector.
    fn capabilities(&self) -> ConnectorCapabilities;

    /// Does the connector have this capability?
    fn has_capability(&self, capability: ConnectorCapability) -> bool {
        self.capabilities().contains(capability)
    }

    /// The referential actions supported by the connector.
    fn referential_actions(&self) -> BitFlags<ReferentialAction>;

    /// The referential actions supported when using relationMode = "prisma" by the connector.
    /// There are in fact scenarios in which the set of emulated referential actions supported may change
    /// depending on the connector. For example, Postgres' NoAction mode behaves similarly to Restrict
    /// (raising an error if any referencing rows still exist when the constraint is checked), but with
    /// a subtle twist we decided not to emulate: NO ACTION allows the check to be deferred until later
    /// in the transaction, whereas RESTRICT does not.
    fn emulated_referential_actions(&self) -> BitFlags<ReferentialAction> {
        RelationMode::allowed_emulated_referential_actions_default()
    }

    fn supports_referential_action(&self, relation_mode: &RelationMode, action: ReferentialAction) -> bool {
        match relation_mode {
            RelationMode::ForeignKeys => self.referential_actions().contains(action),
            RelationMode::Prisma => self.emulated_referential_actions().contains(action),
        }
    }

    /// This is used by the query engine schema builder.
    ///
    /// For a given scalar type + native type combination, this method should return the name to be
    /// given to the filter input objects for the type. The significance of that name is that the
    /// resulting input objects will be cached by name, so for a given filter input object name,
    /// the filters should always be identical.
    fn scalar_filter_name(&self, scalar_type_name: String, _native_type_name: Option<&str>) -> Cow<'_, str> {
        Cow::Owned(scalar_type_name)
    }

    /// This is used by the query engine schema builder. It is only called for filters of String
    /// fields and aggregates.
    ///
    /// For a given filter input object type name returned by `scalar_filter_name`, it should
    /// return the string operations to be made available in the Client API.
    ///
    /// Implementations of this method _must_ always associate the same filters to the same input
    /// object type name. This is because the filter types are cached by name, so if different
    /// calls to the method return different filters, only the first return value will be used.
    fn string_filters(&self, input_object_name: &str) -> BitFlags<StringFilter> {
        match input_object_name {
            "String" => BitFlags::all(), // all the filters are available by default
            _ => panic!("Unexpected scalar input object name for string filters: `{input_object_name}`"),
        }
    }

    /// Debug/error representation of a native type.
    fn native_type_to_parts(&self, native_type: &NativeTypeInstance) -> (&'static str, Vec<String>);

    /// This function is used during Schema parsing to calculate the concrete native type.
    /// It is also used by the Query Engine to parse the native type of a field.
    fn parse_native_type(
        &self,
        name: &str,
        args: &[String],
        span: Span,
        diagnostics: &mut Diagnostics,
    ) -> Option<NativeTypeInstance>;

    fn parse_json_datetime(
        &self,
        _str: &str,
        _nt: Option<NativeTypeInstance>,
    ) -> chrono::ParseResult<DateTime<FixedOffset>> {
        unreachable!("This method is only implemented on connectors with lateral join support.")
    }
}

/// The datamodel connector API.
pub trait Connector: Send + Sync + ValidatedConnector {
    /// The database flavour, divergences in database backends capabilities might consider
    /// us to use a different flavour, like in the case of CockroachDB. However other databases
    /// are less divergent as to consider sharing a flavour with others, like Planetscale and MySQL
    /// or Neon and Postgres, which respectively have the Mysql and Postgres flavours.
    /// Note: this is not used in any `query-engine`.
    fn flavour(&self) -> Flavour;

    /// The maximum length of constraint names in bytes. Connectors without a
    /// limit should return usize::MAX.
    /// Note: this is not used in any `query-engine`.
    fn max_identifier_length(&self) -> usize;

    // Relation mode

    /// The relation modes that can be set through the relationMode datasource
    /// argument.
    /// Note: this is not used in any `query-engine`.
    fn allowed_relation_mode_settings(&self) -> BitFlags<RelationMode> {
        use RelationMode::*;

        ForeignKeys | Prisma
    }

    /// The default relation mode to assume for this connector.
    /// Note: this is not used in any `query-engine`.
    fn default_relation_mode(&self) -> RelationMode {
        RelationMode::ForeignKeys
    }

    /// Most SQL databases reject table definitions with a SET NULL referential action referencing a non-nullable field,
    /// but that's not true for all of them.
    /// This was introduced because Postgres accepts data definition language statements with the SET NULL
    /// referential action referencing non-nullable fields, although this would lead to a runtime error once
    /// the action is actually triggered.
    /// Note: this is not used in any `query-engine`.
    fn allows_set_null_referential_action_on_non_nullable_fields(&self, _relation_mode: RelationMode) -> bool {
        false
    }

    /// Note: this is not used in any `query-engine`.
    fn supports_composite_types(&self) -> bool {
        self.has_capability(ConnectorCapability::CompositeTypes)
    }

    /// Note: this is not used in any `query-engine`.
    fn supports_named_primary_keys(&self) -> bool {
        self.has_capability(ConnectorCapability::NamedPrimaryKeys)
    }

    /// Note: this is not used in any `query-engine`.
    fn supports_named_foreign_keys(&self) -> bool {
        self.has_capability(ConnectorCapability::NamedForeignKeys)
    }

    /// Note: this is not used in any `query-engine`.
    fn supports_named_default_values(&self) -> bool {
        self.has_capability(ConnectorCapability::NamedDefaultValues)
    }

    /// Validate that the arguments passed to a native type attribute are valid.
    /// Note: this is not used in any `query-engine`.
    fn validate_native_type_arguments(
        &self,
        _native_type: &NativeTypeInstance,
        _scalar_type: &ScalarType,
        _span: Span,
        _: &mut Diagnostics,
    ) {
    }

    /// Note: this is not used in any `query-engine`.
    fn validate_enum(&self, _enum: walkers::EnumWalker<'_>, _: &mut Diagnostics) {}
    /// Note: this is not used in any `query-engine`.
    fn validate_model(&self, _model: walkers::ModelWalker<'_>, _: RelationMode, _: &mut Diagnostics) {}
    /// Note: this is not used in any `query-engine`.
    fn validate_relation_field(&self, _field: walkers::RelationFieldWalker<'_>, _: &mut Diagnostics) {}
    /// Note: this is not used in any `query-engine`.
    fn validate_datasource(&self, _: BitFlags<PreviewFeature>, _: &Datasource, _: &mut Diagnostics) {}

    /// Note: this is not used in any `query-engine`.
    fn validate_scalar_field_unknown_default_functions(
        &self,
        db: &parser_database::ParserDatabase,
        diagnostics: &mut Diagnostics,
    ) {
        for d in db.walk_scalar_field_defaults_with_unknown_function() {
            let (func_name, _, span) = d.value().as_function().unwrap();
            diagnostics.push_error(DatamodelError::new_default_unknown_function(func_name, span));
        }
    }

    /// The scopes in which a constraint name should be validated. If empty, doesn't check for name
    /// clashes in the validation phase.
    /// Note: this is not used in any `query-engine`.
    fn constraint_violation_scopes(&self) -> &'static [ConstraintScope] {
        &[]
    }

    /// Returns all available native type constructors available through this connector.
    /// Powers the auto completion of the VSCode plugin.
    /// Note: this is not used in any `query-engine`.
    fn available_native_type_constructors(&self) -> &'static [NativeTypeConstructor];

    /// Returns the default scalar type for the given native type
    /// Note: this is not used in any `query-engine`.
    fn scalar_type_for_native_type(&self, native_type: &NativeTypeInstance) -> ScalarType;

    /// On each connector, each built-in Prisma scalar type (`Boolean`,
    /// `String`, `Float`, etc.) has a corresponding native type.
    /// Note: this is not used in any `query-engine`.
    fn default_native_type_for_scalar_type(&self, scalar_type: &ScalarType) -> NativeTypeInstance;

    /// Same mapping as `default_native_type_for_scalar_type()`, but in the opposite direction.
    /// Note: this is not used in any `query-engine`.
    fn native_type_is_default_for_scalar_type(
        &self,
        native_type: &NativeTypeInstance,
        scalar_type: &ScalarType,
    ) -> bool;

    /// Note: this is not used in any `query-engine`.
    fn find_native_type_constructor(&self, name: &str) -> Option<&NativeTypeConstructor> {
        self.available_native_type_constructors()
            .iter()
            .find(|constructor| constructor.name == name)
    }

    /// Note: this is not used in any `query-engine`.
    fn supports_scalar_lists(&self) -> bool {
        self.has_capability(ConnectorCapability::ScalarLists)
    }

    /// Note: this is not used in any `query-engine`.
    fn supports_enums(&self) -> bool {
        self.has_capability(ConnectorCapability::Enums)
    }

    /// Note: this is not used in any `query-engine`.
    fn supports_json(&self) -> bool {
        self.has_capability(ConnectorCapability::Json)
    }

    /// Note: this is not used in any `query-engine`.
    fn supports_json_lists(&self) -> bool {
        self.has_capability(ConnectorCapability::JsonLists)
    }

    /// Note: this is not used in any `query-engine`.
    fn supports_auto_increment(&self) -> bool {
        self.has_capability(ConnectorCapability::AutoIncrement)
    }

    /// Note: this is not used in any `query-engine`.
    fn supports_non_id_auto_increment(&self) -> bool {
        self.has_capability(ConnectorCapability::AutoIncrementAllowedOnNonId)
    }

    /// Note: this is not used in any `query-engine`.
    fn supports_multiple_auto_increment(&self) -> bool {
        self.has_capability(ConnectorCapability::AutoIncrementMultipleAllowed)
    }

    /// Note: this is not used in any `query-engine`.
    fn supports_non_indexed_auto_increment(&self) -> bool {
        self.has_capability(ConnectorCapability::AutoIncrementNonIndexedAllowed)
    }

    /// Note: this is not used in any `query-engine`.
    fn supports_compound_ids(&self) -> bool {
        self.has_capability(ConnectorCapability::CompoundIds)
    }

    /// Note: this is not used in any `query-engine`.
    fn supports_decimal(&self) -> bool {
        self.has_capability(ConnectorCapability::DecimalType)
    }

    /// Note: this is not used in any `query-engine`.
    fn supported_index_types(&self) -> BitFlags<IndexAlgorithm> {
        IndexAlgorithm::BTree.into()
    }

    /// Note: this is not used in any `query-engine`.
    fn supports_index_type(&self, algo: IndexAlgorithm) -> bool {
        self.supported_index_types().contains(algo)
    }

    /// Note: this is not used in any `query-engine`.
    fn allows_relation_fields_in_arbitrary_order(&self) -> bool {
        self.has_capability(ConnectorCapability::RelationFieldsInArbitraryOrder)
    }

    /// If true, the schema validator function checks whether the referencing fields in a `@relation` attribute
    /// are included in an index.
    /// Note: this is not used in any `query-engine`.
    fn should_suggest_missing_referencing_fields_indexes(&self) -> bool {
        true
    }

    /// Note: this is not used in any `query-engine`.
    fn native_type_to_string(&self, instance: &NativeTypeInstance) -> String {
        let (name, args) = self.native_type_to_parts(instance);
        let args = if args.is_empty() {
            String::new()
        } else {
            format!("({})", args.join(","))
        };
        format!("{name}{args}")
    }

    /// Note: this is not used in any `query-engine`.
    fn native_instance_error(&self, instance: &NativeTypeInstance) -> NativeTypeErrorFactory {
        NativeTypeErrorFactory::new(self.native_type_to_string(instance), self.name().to_owned())
    }

    /// Note: this is not used in any `query-engine`.
    fn validate_url(&self, url: &str) -> Result<(), String>;

    /// Note: this is not used in any `query-engine`.
    fn datamodel_completions(
        &self,
        _db: &ParserDatabase,
        _position: SchemaPosition<'_>,
        _completions: &mut CompletionList,
    ) {
    }

    /// Note: this is not used in any `query-engine`.
    fn datasource_completions(&self, _config: &Configuration, _completion_list: &mut CompletionList) {}

    /// Note: this is not used in any `query-engine`.
    fn parse_datasource_properties(
        &self,
        _args: &mut HashMap<&str, (Span, &ast::Expression)>,
        _diagnostics: &mut Diagnostics,
    ) -> DatasourceConnectorData {
        Default::default()
    }
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub enum Flavour {
    Cockroach,
    Mongo,
    Sqlserver,
    Mysql,
    Postgres,
    Sqlite,
}

impl FromStr for Flavour {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "mysql" => Ok(Self::Mysql),
            "postgres" => Ok(Self::Postgres),
            "cockroachdb" => Ok(Self::Cockroach),
            "mssql" => Ok(Self::Sqlserver),
            "sqlite" => Ok(Self::Sqlite),
            _ => Err(format!("Unknown flavour: {}", s)),
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd)]
pub enum ConstraintType {
    PrimaryKey,
    ForeignKey,
    KeyOrIdx,
    Default,
}

/// A scope where a constraint name must be unique.
#[derive(Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Clone, Copy)]
pub enum ConstraintScope {
    /// Globally indices and unique constraints
    GlobalKeyIndex,
    /// Globally foreign keys
    GlobalForeignKey,
    /// Globally primary keys, indices and unique constraints
    GlobalPrimaryKeyKeyIndex,
    /// Globally primary keys, foreign keys and default constraints
    GlobalPrimaryKeyForeignKeyDefault,
    /// Per model indices and unique constraints
    ModelKeyIndex,
    /// Per model primary keys, indices and unique constraints
    ModelPrimaryKeyKeyIndex,
    /// Per model primary keys, foreign keys, indices and unique constraints
    ModelPrimaryKeyKeyIndexForeignKey,
}

impl ConstraintScope {
    /// A beefed-up display for errors.
    pub fn description(self, model_name: &str) -> Cow<'static, str> {
        match self {
            ConstraintScope::GlobalKeyIndex => Cow::from("global for indexes and unique constraints"),
            ConstraintScope::GlobalForeignKey => Cow::from("global for foreign keys"),
            ConstraintScope::GlobalPrimaryKeyKeyIndex => {
                Cow::from("global for primary key, indexes and unique constraints")
            }
            ConstraintScope::GlobalPrimaryKeyForeignKeyDefault => {
                Cow::from("global for primary keys, foreign keys and default constraints")
            }
            ConstraintScope::ModelKeyIndex => {
                Cow::from(format!("on model `{model_name}` for indexes and unique constraints"))
            }
            ConstraintScope::ModelPrimaryKeyKeyIndex => Cow::from(format!(
                "on model `{model_name}` for primary key, indexes and unique constraints"
            )),
            ConstraintScope::ModelPrimaryKeyKeyIndexForeignKey => Cow::from(format!(
                "on model `{model_name}` for primary key, indexes, unique constraints and foreign keys"
            )),
        }
    }
}
