// Copyright 2020 Google LLC
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//    https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::collections::{HashMap, HashSet};

use quote::quote;
use syn::{parse_quote, ForeignItem, Item, ItemForeignMod, ItemMod};

use crate::known_types::KNOWN_TYPES;
use crate::{
    additional_cpp_generator::AdditionalNeed,
    types::{make_ident, Namespace, TypeName},
};

use super::{
    api::{Api, Use},
    namespace_organizer::NamespaceEntries,
};

unzip_n::unzip_n!(pub 5);

pub(crate) struct CodegenResults {
    pub(crate) items: Vec<Item>,
    pub(crate) additional_cpp_needs: Vec<AdditionalNeed>,
}

fn remove_nones<T>(input: Vec<Option<T>>) -> Vec<T> {
    input.into_iter().flatten().collect()
}

/// Type which handles generation of code.
/// "Code" here includes a list of Items to expose in Rust,
/// and also a list of "additional C++ needs" which can be passed
/// to the C++ code generator.
/// In practice, much of the "generation" involves connecting together
/// existing lumps of code within the Api structures.
pub(crate) struct CodeGenerator<'a> {
    include_list: &'a [String],
    use_stmts_by_mod: HashMap<Namespace, Vec<Item>>,
    bindgen_mod: ItemMod,
}

impl<'a> CodeGenerator<'a> {
    /// Generate code for a set of APIs that was discovered during parsing.
    pub(crate) fn generate_code(
        all_apis: Vec<Api>,
        include_list: &'a [String],
        use_stmts_by_mod: HashMap<Namespace, Vec<Item>>,
        bindgen_mod: ItemMod,
    ) -> CodegenResults {
        let c = Self {
            include_list,
            use_stmts_by_mod,
            bindgen_mod,
        };
        c.codegen(all_apis)
    }

    fn codegen(mut self, all_apis: Vec<Api>) -> CodegenResults {
        // ... and now let's start to generate the output code.
        // First, the hierarchy of mods containing lots of 'use' statements
        // which is the final API exposed as 'ffi'.
        let mut use_statements = Self::generate_final_use_statements(&all_apis);
        // Next, the (modified) bindgen output, which we include in the
        // output as a 'bindgen' sub-mod.
        let bindgen_root_items = self.generate_final_bindgen_mods(&all_apis);
        // Both of the above are organized into sub-mods by namespace.
        // From here on, things are flat.
        let (extern_c_mod_items, all_items, bridge_items, additional_cpp_needs, deps) = all_apis
            .into_iter()
            .map(|api| {
                (
                    api.extern_c_mod_item,
                    api.global_items,
                    api.bridge_items,
                    api.additional_cpp,
                    api.deps,
                )
            })
            .unzip_n_vec();
        // Items for the [cxx::bridge] mod...
        let mut bridge_items: Vec<Item> = bridge_items.into_iter().flatten().collect();
        // Things to include in the "extern "C"" mod passed within the cxx::bridge
        let mut extern_c_mod_items = remove_nones(extern_c_mod_items);
        // And a list of global items to include at the top level.
        let mut all_items: Vec<Item> = all_items.into_iter().flatten().collect();
        // And finally any C++ we need to generate. And by "we" I mean autocxx not cxx.
        let mut additional_cpp_needs = remove_nones(additional_cpp_needs);
        // Determine what variably-sized C types (e.g. int) we need to include
        self.append_ctype_information(&deps, &mut extern_c_mod_items, &mut additional_cpp_needs);
        extern_c_mod_items
            .extend(self.build_include_foreign_items(!additional_cpp_needs.is_empty()));
        // We will always create an extern "C" mod even if bindgen
        // didn't generate one, e.g. because it only generated types.
        // We still want cxx to know about those types.
        let mut extern_c_mod: ItemForeignMod = parse_quote!(
            extern "C++" {}
        );
        extern_c_mod.items.append(&mut extern_c_mod_items);
        bridge_items.push(Self::make_foreign_mod_unsafe(extern_c_mod));
        // The extensive use of parse_quote here could end up
        // being a performance bottleneck. If so, we might want
        // to set the 'contents' field of the ItemMod
        // structures directly.
        if !bindgen_root_items.is_empty() {
            self.bindgen_mod.content.as_mut().unwrap().1 = vec![Item::Mod(parse_quote! {
                pub mod root {
                    #(#bindgen_root_items)*
                }
            })];
            all_items.push(Item::Mod(self.bindgen_mod));
        }
        all_items.push(Item::Mod(parse_quote! {
            #[cxx::bridge]
            pub mod cxxbridge {
                #(#bridge_items)*
            }
        }));
        all_items.append(&mut use_statements);
        CodegenResults {
            items: all_items,
            additional_cpp_needs,
        }
    }

    fn make_foreign_mod_unsafe(ifm: ItemForeignMod) -> Item {
        // At the moment syn does not support outputting 'unsafe extern "C"' except in verbatim
        // items. See https://github.com/dtolnay/syn/pull/938
        Item::Verbatim(quote! {
            unsafe #ifm
        })
    }

    fn append_ctype_information(
        &self,
        deps: &[HashSet<TypeName>],
        extern_c_mod_items: &mut Vec<ForeignItem>,
        additional_cpp_needs: &mut Vec<AdditionalNeed>,
    ) {
        let ctypes: HashSet<_> = deps
            .iter()
            .flatten()
            .filter(|ty| KNOWN_TYPES.is_ctype(ty))
            .collect();
        for ctype in ctypes {
            let id = make_ident(ctype.get_final_ident());
            let ts = quote! {
                type #id = autocxx::#id;
            };
            extern_c_mod_items.push(ForeignItem::Verbatim(ts));
            additional_cpp_needs.push(AdditionalNeed::CTypeTypedef(ctype.clone()))
        }
    }

    fn build_include_foreign_items(&self, has_additional_cpp_needs: bool) -> Vec<ForeignItem> {
        let extra_inclusion = if has_additional_cpp_needs {
            Some("autocxxgen.h".to_string())
        } else {
            None
        };
        let chained = self.include_list.iter().chain(extra_inclusion.iter());
        chained
            .map(|inc| {
                ForeignItem::Macro(parse_quote! {
                    include!(#inc);
                })
            })
            .collect()
    }

    /// Generate lots of 'use' statements to pull cxxbridge items into the output
    /// mod hierarchy according to C++ namespaces.
    fn generate_final_use_statements(input_items: &[Api]) -> Vec<Item> {
        let mut output_items = Vec::new();
        let ns_entries = NamespaceEntries::new(input_items);
        Self::append_child_use_namespace(&ns_entries, &mut output_items);
        output_items
    }

    fn append_child_use_namespace(ns_entries: &NamespaceEntries, output_items: &mut Vec<Item>) {
        for item in ns_entries.entries() {
            let id = &item.id;
            match &item.use_stmt {
                Use::UsedWithAlias(alias) => output_items.push(Item::Use(parse_quote!(
                    pub use cxxbridge :: #id as #alias;
                ))),
                Use::Used => output_items.push(Item::Use(parse_quote!(
                    pub use cxxbridge :: #id;
                ))),
                Use::Unused => {}
            };
        }
        for (child_name, child_ns_entries) in ns_entries.children() {
            let child_id = make_ident(child_name);
            let mut new_mod: ItemMod = parse_quote!(
                pub mod #child_id {
                    use super::cxxbridge;
                }
            );
            Self::append_child_use_namespace(
                child_ns_entries,
                &mut new_mod.content.as_mut().unwrap().1,
            );
            output_items.push(Item::Mod(new_mod));
        }
    }

    fn append_uses_for_ns(&mut self, items: &mut Vec<Item>, ns: &Namespace) {
        let mut use_stmts = self.use_stmts_by_mod.remove(&ns).unwrap_or_default();
        items.append(&mut use_stmts);
    }

    fn append_child_bindgen_namespace(
        &mut self,
        ns_entries: &NamespaceEntries,
        output_items: &mut Vec<Item>,
        ns: &Namespace,
    ) {
        let mut impl_entries_by_type: HashMap<_, Vec<_>> = HashMap::new();
        for item in ns_entries.entries() {
            output_items.extend(item.bindgen_mod_item.iter().cloned());
            if let Some(impl_entry) = &item.impl_entry {
                impl_entries_by_type
                    .entry(item.id.clone())
                    .or_default()
                    .push(impl_entry);
            }
        }
        for (ty, entries) in impl_entries_by_type.into_iter() {
            output_items.push(Item::Impl(parse_quote! {
                impl #ty {
                    #(#entries)*
                }
            }))
        }
        for (child_name, child_ns_entries) in ns_entries.children() {
            let new_ns = ns.push((*child_name).clone());
            let child_id = make_ident(child_name);

            let mut inner_output_items = Vec::new();
            self.append_child_bindgen_namespace(child_ns_entries, &mut inner_output_items, &new_ns);
            if !inner_output_items.is_empty() {
                let mut new_mod: ItemMod = parse_quote!(
                    pub mod #child_id {
                    }
                );
                self.append_uses_for_ns(&mut inner_output_items, &new_ns);
                new_mod.content.as_mut().unwrap().1 = inner_output_items;
                output_items.push(Item::Mod(new_mod));
            }
        }
    }

    fn generate_final_bindgen_mods(&mut self, input_items: &[Api]) -> Vec<Item> {
        let mut output_items = Vec::new();
        let ns = Namespace::new();
        let ns_entries = NamespaceEntries::new(input_items);
        self.append_child_bindgen_namespace(&ns_entries, &mut output_items, &ns);
        self.append_uses_for_ns(&mut output_items, &ns);
        output_items
    }
}
