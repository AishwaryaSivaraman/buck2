/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use buck2_node::attrs::configured_attr::ConfiguredAttr;
use buck2_node::attrs::display::AttrDisplayWithContextExt;
use buck2_node::attrs::inspect_options::AttrInspectOptions;
use buck2_node::attrs::internal::NAME_ATTRIBUTE_FIELD;
use buck2_node::nodes::configured::ConfiguredTargetNode;
use buck2_node::visibility::VisibilityPattern;
use buck2_node::visibility::VisibilityPatternList;
use buck2_query::query::environment::QueryTarget;
use flatbuffers::FlatBufferBuilder;
use flatbuffers::WIPOffset;
use gazebo::prelude::SliceExt;

mod fbs {
    pub use crate::explain_generated::explain::BoolAttr;
    pub use crate::explain_generated::explain::BoolAttrArgs;
    pub use crate::explain_generated::explain::Build;
    pub use crate::explain_generated::explain::BuildArgs;
    pub use crate::explain_generated::explain::ConfiguredTargetNode;
    pub use crate::explain_generated::explain::ConfiguredTargetNodeArgs;
    pub use crate::explain_generated::explain::DictOfStringsAttr;
    pub use crate::explain_generated::explain::DictOfStringsAttrArgs;
    pub use crate::explain_generated::explain::IntAttr;
    pub use crate::explain_generated::explain::IntAttrArgs;
    pub use crate::explain_generated::explain::ListOfStringsAttr;
    pub use crate::explain_generated::explain::ListOfStringsAttrArgs;
    pub use crate::explain_generated::explain::StringAttr;
    pub use crate::explain_generated::explain::StringAttrArgs;
}

enum AttrField<'a> {
    Bool(&'a str, bool),
    Int(&'a str, i64),
    String(&'a str, String),
    StringList(&'a str, Vec<String>),
    StringDict(&'a str, Vec<(String, String)>),
}

pub(crate) fn gen_fbs(
    data: Vec<ConfiguredTargetNode>,
) -> anyhow::Result<FlatBufferBuilder<'static>> {
    let mut builder = FlatBufferBuilder::new();

    let targets: Result<Vec<_>, _> = data
        .iter()
        .map(|node| target_to_fbs(&mut builder, node))
        .collect();

    let targets = builder.create_vector(&targets?);
    let build = fbs::Build::create(
        &mut builder,
        &fbs::BuildArgs {
            targets: Some(targets),
        },
    );
    builder.finish(build, None);
    Ok(builder)
}

fn target_to_fbs<'a>(
    builder: &'_ mut FlatBufferBuilder<'static>,
    node: &'_ ConfiguredTargetNode,
) -> anyhow::Result<WIPOffset<fbs::ConfiguredTargetNode<'a>>, anyhow::Error> {
    // special attrs
    let name = builder.create_shared_string(&node.name());
    let label = builder.create_shared_string(&node.label().to_string());
    let oncall = node.oncall().map(|v| builder.create_shared_string(v));
    let type_ = builder.create_shared_string(node.rule_type().name());
    let package = builder.create_shared_string(&node.buildfile_path().to_string());
    let target_configuration =
        builder.create_shared_string(&node.target_configuration().to_string());
    let execution_platform = builder.create_shared_string(&node.execution_platform()?.id());
    let deps = list_of_strings_to_fbs(
        builder,
        node.deps().map(|dep| dep.label().to_string()).collect(),
    );
    let plugins = list_of_strings_to_fbs(
        builder,
        node.plugin_lists()
            .iter()
            .map(|(kind, _, _)| kind.to_string())
            .collect(),
    );

    // defined attrs
    let attrs: Vec<_> = node
        .attrs(AttrInspectOptions::DefinedOnly)
        .filter(|a| a.name != NAME_ATTRIBUTE_FIELD)
        .map(|a| categorize(a.value, a.name))
        .collect();

    let list: Vec<_> = attrs
        .iter()
        .filter_map(|attr| match attr {
            AttrField::Bool(n, v) => Some((n, v)),
            _ => None,
        })
        .map(|(key, value)| {
            let key = Some(builder.create_shared_string(key));
            fbs::BoolAttr::create(builder, &fbs::BoolAttrArgs { key, value: *value })
        })
        .collect();
    let bool_attrs = Some(builder.create_vector(&list));

    let list: Vec<_> = attrs
        .iter()
        .filter_map(|attr| match attr {
            AttrField::Int(n, v) => Some((n, v)),
            _ => None,
        })
        .map(|(key, value)| {
            let key = Some(builder.create_shared_string(key));
            fbs::IntAttr::create(builder, &fbs::IntAttrArgs { key, value: *value })
        })
        .collect();
    let int_attrs = Some(builder.create_vector(&list));

    let list: Vec<_> = attrs
        .iter()
        .filter_map(|attr| match attr {
            AttrField::String(n, v) => Some((n, v)),
            _ => None,
        })
        .map(|(key, value)| {
            let key = Some(builder.create_shared_string(key));
            let value = Some(builder.create_shared_string(&value));
            fbs::StringAttr::create(builder, &fbs::StringAttrArgs { key, value })
        })
        .collect();
    let string_attrs = Some(builder.create_vector(&list));

    let list: Vec<_> = attrs
        .iter()
        .filter_map(|attr| match attr {
            AttrField::StringList(n, v) => Some((n, v)),
            _ => None,
        })
        .map(|(key, value)| {
            let key = Some(builder.create_shared_string(key));
            let value = list_of_strings_to_fbs(builder, value.to_vec());
            fbs::ListOfStringsAttr::create(builder, &fbs::ListOfStringsAttrArgs { key, value })
        })
        .collect();
    let list_of_strings_attrs = Some(builder.create_vector(&list));

    let list: Vec<_> = attrs
        .iter()
        .filter_map(|attr| match attr {
            AttrField::StringDict(n, v) => Some((n, v)),
            _ => None,
        })
        .map(|(key, value)| {
            let key = Some(builder.create_shared_string(key));
            let value = dict_of_strings_to_fbs(builder, value.to_vec());
            fbs::DictOfStringsAttr::create(builder, &fbs::DictOfStringsAttrArgs { key, value })
        })
        .collect();
    let dict_of_strings_attrs = Some(builder.create_vector(&list));

    let target = fbs::ConfiguredTargetNode::create(
        builder,
        &fbs::ConfiguredTargetNodeArgs {
            name: Some(name),
            // special attrs
            configured_target_label: Some(label),
            type_: Some(type_),
            deps,
            package: Some(package),
            oncall,
            target_configuration: Some(target_configuration),
            execution_platform: Some(execution_platform),
            plugins,
            // defined attrs
            bool_attrs,
            int_attrs,
            string_attrs,
            list_of_strings_attrs,
            dict_of_strings_attrs,
        },
    );
    Ok(target)
}

fn categorize<'a>(a: ConfiguredAttr, name: &'a str) -> AttrField<'a> {
    match a {
        ConfiguredAttr::Bool(v) => AttrField::Bool(name, v.0),
        ConfiguredAttr::String(v) => AttrField::String(name, v.0.to_string()),
        ConfiguredAttr::List(v) => {
            let mut list = vec![];
            v.0.iter().for_each(|v| {
                match v {
                    ConfiguredAttr::String(v) => list.push(v.0.to_string()),
                    _ => list.push(
                        v.as_display_no_ctx()
                            .to_string()
                            .trim_matches('"')
                            .to_owned(),
                    ), // TODO iguridi: make a "printer_for_explain" for attrs
                }
            });
            AttrField::StringList(name, list)
        }
        ConfiguredAttr::None => AttrField::String(name, "null".to_owned()),
        ConfiguredAttr::Visibility(v) => {
            let list = match v.0 {
                VisibilityPatternList::Public => vec![VisibilityPattern::PUBLIC.to_owned()],
                VisibilityPatternList::List(patterns) => patterns.map(|p| p.to_string()),
            };
            AttrField::StringList(name, list)
        }
        ConfiguredAttr::Int(v) => AttrField::Int(name, v),
        ConfiguredAttr::EnumVariant(v) => AttrField::String(name, v.0.to_string()),
        ConfiguredAttr::Tuple(v) => {
            let mut list = vec![];
            v.0.iter().for_each(|v| {
                match v {
                    ConfiguredAttr::String(v) => list.push(v.0.to_string()),
                    _ => list.push(
                        v.as_display_no_ctx()
                            .to_string()
                            .trim_matches('"')
                            .to_owned(),
                    ), // TODO iguridi: make a "printer_for_explain" for attrs
                }
            });
            AttrField::StringList(name, list)
        }
        ConfiguredAttr::Dict(v) => {
            let string_pairs: Vec<_> =
                v.0.iter()
                    .map(|(k, v)| match (k, v) {
                        (ConfiguredAttr::String(k), ConfiguredAttr::String(v)) => {
                            (k.0.to_string(), v.0.to_string())
                        }
                        _ => (
                            k.as_display_no_ctx()
                                .to_string()
                                .trim_matches('"')
                                .to_owned(),
                            v.as_display_no_ctx()
                                .to_string()
                                .trim_matches('"')
                                .to_owned(),
                        ), // TODO iguridi: make a "printer_for_explain" for attrs
                    })
                    .collect();
            AttrField::StringDict(name, string_pairs)
        }
        ConfiguredAttr::OneOf(v, _) => categorize(*v, name),
        ConfiguredAttr::WithinView(v) => {
            let list = match v.0 {
                VisibilityPatternList::Public => vec![VisibilityPattern::PUBLIC.to_owned()],
                VisibilityPatternList::List(patterns) => patterns.map(|p| p.to_string()),
            };
            AttrField::StringList(name, list)
        }
        ConfiguredAttr::ExplicitConfiguredDep(v) => AttrField::String(name, v.to_string()), // TODO iguridi: structure this
        ConfiguredAttr::SplitTransitionDep(v) => AttrField::String(name, v.to_string()), // TODO iguridi: structure this
        ConfiguredAttr::ConfigurationDep(v) => AttrField::String(name, v.to_string()),
        ConfiguredAttr::PluginDep(v, _) => AttrField::String(name, v.to_string()),
        ConfiguredAttr::Dep(v) => {
            // TODO iguridi: make fbs type for labels
            AttrField::String(name, v.to_string())
        }
        ConfiguredAttr::SourceLabel(v) => AttrField::String(name, v.to_string()),
        ConfiguredAttr::Label(v) => AttrField::String(name, v.to_string()),
        ConfiguredAttr::Arg(v) => AttrField::String(name, v.to_string()),
        ConfiguredAttr::Query(v) => AttrField::String(name, v.query.query),
        ConfiguredAttr::SourceFile(v) => AttrField::String(name, v.path().to_string()),
        ConfiguredAttr::Metadata(v) => AttrField::String(name, v.to_string()),
    }
}

fn list_of_strings_to_fbs<'a>(
    builder: &'_ mut FlatBufferBuilder<'static>,
    list: Vec<String>,
) -> Option<WIPOffset<flatbuffers::Vector<'static, flatbuffers::ForwardsUOffset<&'a str>>>> {
    let list = list
        .into_iter()
        .map(|v| builder.create_shared_string(&v))
        .collect::<Vec<WIPOffset<&str>>>();
    Some(builder.create_vector(&list))
}

fn dict_of_strings_to_fbs<'a>(
    builder: &'_ mut FlatBufferBuilder<'static>,
    dict: Vec<(String, String)>,
) -> Option<
    WIPOffset<flatbuffers::Vector<'static, flatbuffers::ForwardsUOffset<fbs::StringAttr<'a>>>>,
> {
    let list: Vec<WIPOffset<fbs::StringAttr>> = dict
        .into_iter()
        .map(|(key, value)| {
            let key = Some(builder.create_shared_string(&key));
            let value = Some(builder.create_shared_string(&value));
            fbs::StringAttr::create(builder, &fbs::StringAttrArgs { key, value })
        })
        .collect();
    Some(builder.create_vector(&list))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use buck2_core::cells::cell_path::CellPath;
    use buck2_core::configuration::data::ConfigurationData;
    use buck2_core::execution_types::execution::ExecutionPlatform;
    use buck2_core::execution_types::execution::ExecutionPlatformResolution;
    use buck2_core::execution_types::executor_config::CommandExecutorConfig;
    use buck2_core::package::package_relative_path::PackageRelativePath;
    use buck2_core::package::PackageLabel;
    use buck2_core::plugins::PluginKind;
    use buck2_core::plugins::PluginKindSet;
    use buck2_core::provider::label::ProvidersLabel;
    use buck2_core::provider::label::ProvidersName;
    use buck2_core::target::label::label::TargetLabel;
    use buck2_core::target::name::TargetName;
    use buck2_node::attrs::attr::Attribute;
    use buck2_node::attrs::attr_type::arg::StringWithMacros;
    use buck2_node::attrs::attr_type::bool::BoolLiteral;
    use buck2_node::attrs::attr_type::dep::DepAttr;
    use buck2_node::attrs::attr_type::dep::DepAttrTransition;
    use buck2_node::attrs::attr_type::dep::DepAttrType;
    use buck2_node::attrs::attr_type::dict::DictLiteral;
    use buck2_node::attrs::attr_type::list::ListLiteral;
    use buck2_node::attrs::attr_type::query::QueryAttr;
    use buck2_node::attrs::attr_type::query::QueryAttrBase;
    use buck2_node::attrs::attr_type::query::ResolvedQueryLiterals;
    use buck2_node::attrs::attr_type::string::StringLiteral;
    use buck2_node::attrs::attr_type::tuple::TupleLiteral;
    use buck2_node::attrs::attr_type::AttrType;
    use buck2_node::attrs::coerced_attr::CoercedAttr;
    use buck2_node::attrs::coerced_path::CoercedPath;
    use buck2_node::attrs::internal::METADATA_ATTRIBUTE_FIELD;
    use buck2_node::attrs::internal::VISIBILITY_ATTRIBUTE_FIELD;
    use buck2_node::attrs::internal::WITHIN_VIEW_ATTRIBUTE_FIELD;
    use buck2_node::metadata::key::MetadataKey;
    use buck2_node::metadata::map::MetadataMap;
    use buck2_node::metadata::value::MetadataValue;
    use buck2_node::provider_id_set::ProviderIdSet;
    use buck2_node::visibility::VisibilitySpecification;
    use buck2_node::visibility::WithinViewSpecification;
    use buck2_util::arc_str::ArcSlice;
    use dupe::Dupe;
    use starlark_map::small_map::SmallMap;

    use super::*;
    pub use crate::explain_generated::explain::Build;

    #[test]
    fn test_bool_attr() {
        let data = gen_data(
            vec![(
                "bool_field",
                Attribute::new(None, "", AttrType::bool()),
                CoercedAttr::Bool(BoolLiteral(false)),
            )],
            vec![],
        );

        let fbs = gen_fbs(data).unwrap();
        let fbs = fbs.finished_data();
        let build = flatbuffers::root::<Build>(fbs).unwrap();
        let target = build.targets().unwrap().get(0);

        assert_things(target, build);
        assert_eq!(
            target.bool_attrs().unwrap().get(0).key(),
            Some("bool_field")
        );
    }

    #[test]
    fn test_int_attr() {
        let data = gen_data(
            vec![(
                "int_field",
                Attribute::new(None, "", AttrType::int()),
                CoercedAttr::Int(1),
            )],
            vec![],
        );

        let fbs = gen_fbs(data).unwrap();
        let fbs = fbs.finished_data();
        let build = flatbuffers::root::<Build>(fbs).unwrap();
        let target = build.targets().unwrap().get(0);

        assert_things(target, build);
        assert_eq!(target.int_attrs().unwrap().get(0).key(), Some("int_field"));
        assert_eq!(target.int_attrs().unwrap().get(0).value(), 1);
    }

    #[test]
    fn test_string_attr() {
        let data = gen_data(
            vec![(
                "bar",
                Attribute::new(None, "", AttrType::string()),
                CoercedAttr::String(StringLiteral("foo".into())),
            )],
            vec![],
        );

        let fbs = gen_fbs(data).unwrap();
        let fbs = fbs.finished_data();
        let build = flatbuffers::root::<Build>(fbs).unwrap();
        let target = build.targets().unwrap().get(0);

        assert_things(target, build);
        assert_eq!(target.string_attrs().unwrap().get(0).key(), Some("bar"));
        assert_eq!(target.string_attrs().unwrap().get(0).value(), Some("foo"));
    }

    #[test]
    fn test_enum_attr() -> anyhow::Result<()> {
        let data = gen_data(
            vec![(
                "enum_field",
                Attribute::new(None, "", AttrType::enumeration(vec!["field".to_owned()])?),
                CoercedAttr::EnumVariant(StringLiteral("some_string".into())),
            )],
            vec![],
        );

        let fbs = gen_fbs(data).unwrap();
        let fbs = fbs.finished_data();
        let build = flatbuffers::root::<Build>(fbs).unwrap();
        let target = build.targets().unwrap().get(0);

        assert_things(target, build);
        assert_eq!(
            target.string_attrs().unwrap().get(0).key(),
            Some("enum_field")
        );
        assert_eq!(
            target.string_attrs().unwrap().get(0).value(),
            Some("some_string")
        );
        Ok(())
    }

    #[test]
    fn test_arg_attr() {
        let data = gen_data(
            vec![(
                "bar",
                Attribute::new(None, "", AttrType::arg(false)),
                CoercedAttr::Arg(StringWithMacros::StringPart(
                    "$(location :relative_path_test_file)".into(),
                )),
            )],
            vec![],
        );

        let fbs = gen_fbs(data).unwrap();
        let fbs = fbs.finished_data();
        let build = flatbuffers::root::<Build>(fbs).unwrap();
        let target = build.targets().unwrap().get(0);

        assert_things(target, build);
        assert_eq!(target.string_attrs().unwrap().get(0).key(), Some("bar"));
        assert_eq!(
            target.string_attrs().unwrap().get(0).value(),
            Some("$(location :relative_path_test_file)")
        );
    }

    #[test]
    fn test_source_path_attr() {
        let data = gen_data(
            vec![(
                "bar",
                Attribute::new(None, "", AttrType::source(false)),
                CoercedAttr::SourceFile(CoercedPath::File(
                    PackageRelativePath::new("foo/bar").unwrap().to_arc(),
                )),
            )],
            vec![],
        );

        let fbs = gen_fbs(data).unwrap();
        let fbs = fbs.finished_data();
        let build = flatbuffers::root::<Build>(fbs).unwrap();
        let target = build.targets().unwrap().get(0);

        assert_things(target, build);
        assert_eq!(target.string_attrs().unwrap().get(0).key(), Some("bar"));
        assert_eq!(
            target.string_attrs().unwrap().get(0).value(),
            Some("foo/bar")
        );
    }

    #[test]
    fn test_query_attr() {
        let pkg = PackageLabel::testing_parse("cell//foo/bar");
        let name = TargetName::testing_new("t2");
        let label = TargetLabel::new(pkg, name.as_ref());
        let mut map: BTreeMap<String, ProvidersLabel> = BTreeMap::new();
        map.insert("key1".to_owned(), ProvidersLabel::default_for(label));

        let data = gen_data(
            vec![(
                "bar",
                Attribute::new(None, "", AttrType::query()),
                CoercedAttr::Query(Box::new(QueryAttr {
                    query: QueryAttrBase {
                        query: "$(query_targets deps(:foo))".to_owned(),
                        resolved_literals: ResolvedQueryLiterals(map),
                    },
                    providers: ProviderIdSet::EMPTY,
                })),
            )],
            vec![],
        );

        let fbs = gen_fbs(data).unwrap();
        let fbs = fbs.finished_data();
        let build = flatbuffers::root::<Build>(fbs).unwrap();
        let target = build.targets().unwrap().get(0);

        assert_things(target, build);
        assert_eq!(target.string_attrs().unwrap().get(0).key(), Some("bar"));
        assert_eq!(
            target.string_attrs().unwrap().get(0).value(),
            Some("$(query_targets deps(:foo))")
        );
    }

    #[test]
    fn test_plugin_dep() {
        let pkg = PackageLabel::testing_parse("cell//foo/bar");
        let name = TargetName::testing_new("t2");
        let label = TargetLabel::new(pkg, name.as_ref());
        let data = gen_data(
            vec![(
                "plugin_dep_field",
                Attribute::new(
                    None,
                    "",
                    AttrType::plugin_dep(PluginKind::new(
                        "foo".to_owned(),
                        CellPath::testing_new("cell//foo/bar"),
                    )),
                ),
                CoercedAttr::PluginDep(label),
            )],
            vec![],
        );

        let fbs = gen_fbs(data).unwrap();
        let fbs = fbs.finished_data();
        let build = flatbuffers::root::<Build>(fbs).unwrap();
        let target = build.targets().unwrap().get(0);

        assert_things(target, build);
        assert_eq!(
            target.string_attrs().unwrap().get(0).key(),
            Some("plugin_dep_field")
        );
        assert_eq!(
            target.string_attrs().unwrap().get(0).value(),
            Some("cell//foo/bar:t2")
        );
    }

    fn check_label(
        f: impl Fn(TargetLabel) -> (&'static str, Attribute, CoercedAttr),
    ) -> Result<(), anyhow::Error> {
        let pkg = PackageLabel::testing_parse("cell//foo/bar");
        let name = TargetName::testing_new("t2");
        let label = TargetLabel::new(pkg, name.as_ref());
        let tuple = f(label);
        let data = gen_data(vec![tuple], vec![]);

        let fbs = gen_fbs(data).unwrap();
        let fbs = fbs.finished_data();
        let build = flatbuffers::root::<Build>(fbs).unwrap();
        let target = build.targets().unwrap().get(0);

        assert_things(target, build);
        assert!(
            target
                .string_attrs()
                .unwrap()
                .get(0)
                .value()
                .unwrap()
                .contains("cell//foo/bar:t2 (<testing>#")
        );
        Ok(())
    }

    #[test]
    fn test_deps_attr() -> anyhow::Result<()> {
        let f = |label| {
            (
                "label_field",
                Attribute::new(
                    None,
                    "",
                    AttrType::dep(ProviderIdSet::EMPTY, PluginKindSet::EMPTY),
                ),
                CoercedAttr::Dep(ProvidersLabel::default_for(label)),
            )
        };
        check_label(f)
    }

    #[test]
    fn test_label_attr() -> anyhow::Result<()> {
        let f = |label| {
            (
                "label_field",
                Attribute::new(None, "", AttrType::label()),
                CoercedAttr::Label(ProvidersLabel::default_for(label)),
            )
        };
        check_label(f)
    }

    #[test]
    fn test_source_label_attr() -> anyhow::Result<()> {
        let f = |label| {
            (
                "label_field",
                Attribute::new(None, "", AttrType::source(false)),
                CoercedAttr::SourceLabel(ProvidersLabel::default_for(label)),
            )
        };
        check_label(f)
    }

    #[test]
    fn test_configured_dep_attr() -> anyhow::Result<()> {
        let f = |label: TargetLabel| {
            (
                "label_field",
                Attribute::new(None, "", AttrType::label()),
                CoercedAttr::ConfiguredDep(Box::new(DepAttr {
                    attr_type: DepAttrType::new(
                        ProviderIdSet::EMPTY,
                        DepAttrTransition::Identity(PluginKindSet::EMPTY),
                    ),
                    label: ProvidersLabel::default_for(label)
                        .configure(ConfigurationData::testing_new()),
                })),
            )
        };
        check_label(f)
    }

    #[test]
    fn test_tuple_attr() {
        let data = gen_data(
            vec![(
                "some_tuple",
                Attribute::new(
                    None,
                    "",
                    AttrType::tuple(vec![AttrType::string(), AttrType::string()]),
                ),
                CoercedAttr::Tuple(TupleLiteral(ArcSlice::new([
                    CoercedAttr::String(StringLiteral("some_string1".into())),
                    CoercedAttr::String(StringLiteral("some_string2".into())),
                ]))),
            )],
            vec![],
        );

        let fbs = gen_fbs(data).unwrap();
        let fbs = fbs.finished_data();
        let build = flatbuffers::root::<Build>(fbs).unwrap();
        let target = build.targets().unwrap().get(0);

        assert_things(target, build);
        assert_eq!(
            target
                .list_of_strings_attrs()
                .unwrap()
                .get(0)
                .value()
                .unwrap()
                .get(0),
            "some_string1"
        );
    }

    #[test]
    fn test_list_of_strings() {
        let pkg = PackageLabel::testing_parse("cell//foo/bar");
        let name = TargetName::testing_new("t2");
        let label = TargetLabel::new(pkg, name.as_ref());
        let data = gen_data(
            vec![(
                "some_deps",
                Attribute::new(
                    None,
                    "",
                    AttrType::list(AttrType::dep(ProviderIdSet::EMPTY, PluginKindSet::EMPTY)),
                ),
                CoercedAttr::List(ListLiteral(ArcSlice::new([CoercedAttr::Dep(
                    ProvidersLabel::new(label, ProvidersName::Default),
                )]))),
            )],
            vec![],
        );

        let fbs = gen_fbs(data).unwrap();
        let fbs = fbs.finished_data();
        let build = flatbuffers::root::<Build>(fbs).unwrap();
        let target = build.targets().unwrap().get(0);

        assert_things(target, build);
        assert_eq!(
            target.list_of_strings_attrs().unwrap().get(0).key(),
            Some("some_deps")
        );
    }

    #[test]
    fn test_visibility() {
        let data = gen_data(
            vec![],
            vec![(
                VISIBILITY_ATTRIBUTE_FIELD,
                Attribute::new(None, "", AttrType::visibility()),
                CoercedAttr::Visibility(VisibilitySpecification(VisibilityPatternList::Public)),
            )],
        );

        let fbs = gen_fbs(data).unwrap();
        let fbs = fbs.finished_data();
        let build = flatbuffers::root::<Build>(fbs).unwrap();
        let target = build.targets().unwrap().get(0);

        assert_things(target, build);
        assert_eq!(
            target.list_of_strings_attrs().unwrap().get(0).key(),
            Some(VISIBILITY_ATTRIBUTE_FIELD)
        );
        assert_eq!(
            target
                .list_of_strings_attrs()
                .unwrap()
                .get(0)
                .value()
                .unwrap()
                .get(0),
            "PUBLIC"
        );
    }

    #[test]
    fn test_one_of_attr() {
        let data = gen_data(
            vec![(
                "one_of_field",
                Attribute::new(None, "", AttrType::one_of(vec![AttrType::int()])),
                CoercedAttr::OneOf(Box::new(CoercedAttr::Int(7)), 0),
            )],
            vec![],
        );

        let fbs = gen_fbs(data).unwrap();
        let fbs = fbs.finished_data();
        let build = flatbuffers::root::<Build>(fbs).unwrap();
        let target = build.targets().unwrap().get(0);

        assert_things(target, build);
        assert_eq!(
            target.int_attrs().unwrap().get(0).key(),
            Some("one_of_field")
        );
        assert_eq!(target.int_attrs().unwrap().get(0).value(), 7);
    }

    #[test]
    fn test_within_view() {
        let data = gen_data(
            vec![],
            vec![(
                WITHIN_VIEW_ATTRIBUTE_FIELD,
                Attribute::new(None, "", AttrType::within_view()),
                CoercedAttr::WithinView(WithinViewSpecification(VisibilityPatternList::Public)),
            )],
        );

        let fbs = gen_fbs(data).unwrap();
        let fbs = fbs.finished_data();
        let build = flatbuffers::root::<Build>(fbs).unwrap();
        let target = build.targets().unwrap().get(0);

        assert_things(target, build);
        assert_eq!(
            target.list_of_strings_attrs().unwrap().get(0).key(),
            Some(WITHIN_VIEW_ATTRIBUTE_FIELD)
        );
        assert_eq!(
            target
                .list_of_strings_attrs()
                .unwrap()
                .get(0)
                .value()
                .unwrap()
                .get(0),
            "PUBLIC"
        );
    }

    #[test]
    fn test_dict_of_strings() {
        let data = gen_data(
            vec![(
                "dict_field",
                Attribute::new(
                    None,
                    "",
                    AttrType::dict(AttrType::string(), AttrType::string(), false),
                ),
                CoercedAttr::Dict(DictLiteral(ArcSlice::new([(
                    CoercedAttr::String(StringLiteral("foo".into())),
                    CoercedAttr::String(StringLiteral("bar".into())),
                )]))),
            )],
            vec![],
        );

        let fbs = gen_fbs(data).unwrap();
        let fbs = fbs.finished_data();
        let build = flatbuffers::root::<Build>(fbs).unwrap();
        let target = build.targets().unwrap().get(0);

        assert_things(target, build);
        assert_eq!(
            target.dict_of_strings_attrs().unwrap().get(0).key(),
            Some("dict_field")
        );
        assert_eq!(
            target
                .dict_of_strings_attrs()
                .unwrap()
                .get(0)
                .value()
                .unwrap()
                .get(0)
                .value(),
            Some("bar")
        );
    }

    #[test]
    fn test_metadata_attr() -> anyhow::Result<()> {
        let mut map = SmallMap::new();
        map.insert(
            MetadataKey::try_from("key.something".to_owned())?,
            MetadataValue::new(serde_json::Value::String("foo".to_owned())),
        );
        let data = gen_data(
            vec![],
            vec![(
                METADATA_ATTRIBUTE_FIELD,
                Attribute::new(None, "", AttrType::metadata()),
                CoercedAttr::Metadata(MetadataMap::new(map)),
            )],
        );

        let fbs = gen_fbs(data).unwrap();
        let fbs = fbs.finished_data();
        let build = flatbuffers::root::<Build>(fbs).unwrap();
        let target = build.targets().unwrap().get(0);

        assert_things(target, build);
        assert_eq!(
            target.string_attrs().unwrap().get(0).value(),
            Some("{\"key.something\":\"foo\"}")
        );
        Ok(())
    }

    fn assert_things(target: fbs::ConfiguredTargetNode<'_>, build: fbs::Build<'_>) {
        // special attrs
        assert!(
            target
                .configured_target_label()
                .unwrap()
                .contains("cell//pkg:foo (<testing>#")
        );
        assert_eq!(target.name(), Some("foo"));
        assert_eq!(target.type_(), Some("foo_lib"));
        assert_eq!(target.package(), Some("cell//pkg:BUCK"));
        assert_eq!(target.oncall(), None);
        assert_eq!(target.execution_platform(), Some("cell//pkg:bar"));
        assert_eq!(target.deps().unwrap().is_empty(), true);
        assert_eq!(target.plugins().unwrap().is_empty(), true);

        let target2 = build.targets().unwrap().get(1);
        assert!(
            target2
                .configured_target_label()
                .unwrap()
                .contains("cell//pkg:baz (<testing>#"),
        );
    }

    fn gen_data(
        attrs: Vec<(
            &str,
            buck2_node::attrs::attr::Attribute,
            buck2_node::attrs::coerced_attr::CoercedAttr,
        )>,
        internal_attrs: Vec<(
            &str,
            buck2_node::attrs::attr::Attribute,
            buck2_node::attrs::coerced_attr::CoercedAttr,
        )>,
    ) -> Vec<ConfiguredTargetNode> {
        // Setup data
        let target_label = TargetLabel::testing_parse("cell//pkg:foo");
        let configured_target_label = target_label.configure(ConfigurationData::testing_new());

        let execution_platform_resolution = {
            let platform_label = TargetLabel::testing_parse("cell//pkg:bar");
            let platform = ExecutionPlatform::platform(
                platform_label,
                ConfigurationData::testing_new(),
                CommandExecutorConfig::testing_local(),
            );
            ExecutionPlatformResolution::new(Some(platform), Vec::new())
        };

        let target = ConfiguredTargetNode::testing_new(
            configured_target_label,
            "foo_lib",
            execution_platform_resolution.dupe(),
            attrs,
            internal_attrs,
        );

        let target_label2 = TargetLabel::testing_parse("cell//pkg:baz");
        let configured_target_label2 = target_label2.configure(ConfigurationData::testing_new());
        let target2 = ConfiguredTargetNode::testing_new(
            configured_target_label2,
            "foo_lib",
            execution_platform_resolution,
            vec![],
            vec![],
        );
        vec![target, target2]
    }
}
