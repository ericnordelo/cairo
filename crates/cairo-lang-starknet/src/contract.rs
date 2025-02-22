use anyhow::Context;
use cairo_lang_defs::ids::{
    FreeFunctionId, LanguageElementId, ModuleId, ModuleItemId, SubmoduleId, TraitId,
};
use cairo_lang_diagnostics::ToOption;
use cairo_lang_filesystem::ids::CrateId;
use cairo_lang_semantic::db::SemanticGroup;
use cairo_lang_semantic::plugin::DynPluginAuxData;
use num_bigint::BigUint;
use sha3::{Digest, Keccak256};

use crate::plugin::aux_data::StarkNetContractAuxData;
use crate::plugin::consts::ABI_TRAIT;

#[cfg(test)]
#[path = "contract_test.rs"]
mod test;

/// Represents a declaration of a contract.
pub struct ContractDeclaration {
    /// The id of the module that defines the contract.
    pub submodule_id: SubmoduleId,
}

impl ContractDeclaration {
    pub fn module_id(&self) -> ModuleId {
        ModuleId::Submodule(self.submodule_id)
    }
}

/// A variant of eth-keccak that computes a value that fits in a StarkNet field element.
pub fn starknet_keccak(data: &[u8]) -> BigUint {
    let mut hasher = Keccak256::new();
    hasher.update(data);
    let mut result = hasher.finalize();

    // Truncate result to 250 bits.
    *result.first_mut().unwrap() &= 3;
    BigUint::from_bytes_be(&result)
}

/// Finds the inline modules annotated as contracts in the given crate_ids and
/// returns the corresponding ContractDeclarations.
pub fn find_contracts(db: &dyn SemanticGroup, crate_ids: &[CrateId]) -> Vec<ContractDeclaration> {
    let mut contracts = vec![];
    for crate_id in crate_ids {
        let modules = db.crate_modules(*crate_id);
        for module_id in modules.iter() {
            let generated_file_infos =
                db.module_generated_file_infos(*module_id).unwrap_or_default();

            // When a module is generated by a plugin the same generated_file_info appears in two
            // places:
            //   a. db.module_generated_file_infos(*original_module_id)?[k] (with k > 0).
            //   b. db.module_generated_file_infos(*generated_module_id)?[0].
            // We are interested in modules that the plugin acted on and not modules that were
            // created by the plugin, so we skip generated_file_infos[0].
            // For example if we have
            // mod A {
            //    #[contract]
            //    mod B {
            //    }
            // }
            // Then we want lookup B inside A and not inside B.

            for generated_file_info in generated_file_infos.iter().skip(1) {
                let Some(generated_file_info) = generated_file_info else { continue; };
                let Some(mapper) = generated_file_info.aux_data.0.as_any(
                ).downcast_ref::<DynPluginAuxData>() else { continue; };
                let Some(aux_data) = mapper.0.as_any(
                ).downcast_ref::<StarkNetContractAuxData>() else { continue; };

                for contract_name in &aux_data.contracts {
                    if let Ok(Some(ModuleItemId::Submodule(submodule_id))) =
                        db.module_item_by_name(*module_id, contract_name.clone())
                    {
                        contracts.push(ContractDeclaration { submodule_id });
                    } else {
                        panic!("Contract `{contract_name}` was not found.");
                    }
                }
            }
        }
    }
    contracts
}

/// Returns the list of functions in a given module.
pub fn get_module_functions(
    db: &(dyn SemanticGroup + 'static),
    contract: &ContractDeclaration,
    module_name: &str,
) -> anyhow::Result<Vec<FreeFunctionId>> {
    let generated_module_id = get_generated_contract_module(db, contract)?;
    match db
        .module_item_by_name(generated_module_id, module_name.into())
        .to_option()
        .with_context(|| "Failed to initiate a lookup in the {module_name} module.")?
    {
        Some(ModuleItemId::Submodule(external_module_id)) => Ok(db
            .module_free_functions_ids(ModuleId::Submodule(external_module_id))
            .to_option()
            .with_context(|| "Failed to get external module functions.")?),
        _ => anyhow::bail!("Failed to get the external module."),
    }
}

/// Returns the ABI trait of the given contract.
pub fn get_abi(
    db: &(dyn SemanticGroup + 'static),
    contract: &ContractDeclaration,
) -> anyhow::Result<TraitId> {
    let generated_module_id = get_generated_contract_module(db, contract)?;
    match db
        .module_item_by_name(generated_module_id, ABI_TRAIT.into())
        .to_option()
        .with_context(|| "Failed to initiate a lookup in the generated module.")?
    {
        Some(ModuleItemId::Trait(trait_id)) => Ok(trait_id),
        _ => anyhow::bail!("Failed to get the ABI trait."),
    }
}

/// Returns the generated contract module.
fn get_generated_contract_module(
    db: &(dyn SemanticGroup + 'static),
    contract: &ContractDeclaration,
) -> anyhow::Result<ModuleId> {
    let parent_module_id = contract.submodule_id.parent_module(db.upcast());
    let contract_name = contract.submodule_id.name(db.upcast());

    match db
        .module_item_by_name(parent_module_id, contract_name.clone())
        .to_option()
        .with_context(|| "Failed to initiate a lookup in the root module.")?
    {
        Some(ModuleItemId::Submodule(generated_module_id)) => {
            Ok(ModuleId::Submodule(generated_module_id))
        }
        _ => anyhow::bail!(format!("Failed to get generated module {contract_name}.")),
    }
}
