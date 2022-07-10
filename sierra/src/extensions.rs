use crate::{error::Error, graph::*, mem_state::*};
use std::collections::HashMap;
use Result::*;

mod arithmetic;
mod function_call;
mod gas_station;
mod jump_nz;
mod match_nullable;
mod tuple_obj;
mod unconditional_jump;

#[derive(Debug, PartialEq)]
pub(crate) struct ExtensionSignature {
    pub args: Vec<Type>,
    pub results: Vec<Vec<Type>>,
    pub fallthrough: Option<usize>,
}

fn simple_invoke_ext_sign(args: Vec<Type>, results: Vec<Type>) -> ExtensionSignature {
    ExtensionSignature {
        args: args,
        results: vec![results],
        fallthrough: Some(0),
    }
}

pub(crate) trait ExtensionImplementation {
    fn get_signature(
        self: &Self,
        tmpl_args: &Vec<TemplateArg>,
    ) -> Result<ExtensionSignature, Error>;

    fn mem_change(
        self: &Self,
        tmpl_args: &Vec<TemplateArg>,
        registry: &TypeRegistry,
        mem_state: MemState,
        args_state: &Vec<Location>,
    ) -> Result<(MemState, Vec<Vec<Location>>), Error> {
        let sign = self.get_signature(tmpl_args)?;
        Ok((
            mem_state,
            sign.results
                .iter()
                .map(|r| r.iter().map(|v| Location::Transient).collect())
                .collect(),
        ))
    }
}

type ExtensionBox = Box<dyn ExtensionImplementation + Sync + Send>;

pub(crate) type ExtensionRegistry = HashMap<String, ExtensionBox>;

pub(crate) fn get_ext_registry(prog: &Program) -> ExtensionRegistry {
    chain!(
        arithmetic::extensions().into_iter(),
        function_call::extensions(prog).into_iter(),
        gas_station::extensions().into_iter(),
        jump_nz::extensions().into_iter(),
        match_nullable::extensions().into_iter(),
        tuple_obj::extensions().into_iter(),
        unconditional_jump::extensions().into_iter()
    )
    .collect()
}

pub(crate) fn get_signature(
    registry: &ExtensionRegistry,
    ext: &Extension,
) -> Result<ExtensionSignature, Error> {
    match registry.get(&ext.name) {
        None => Err(Error::UnsupportedLibCallName(ext.name.clone())),
        Some(e) => e.get_signature(&ext.tmpl_args),
    }
}

#[derive(Debug, PartialEq)]
pub(crate) struct TypeInfo {
    pub size: usize,
}

pub(crate) trait TypeInfoImplementation {
    fn get_info(
        self: &Self,
        tmpl_args: &Vec<TemplateArg>,
        registry: &TypeRegistry,
    ) -> Result<TypeInfo, Error>;
}

type TypeInfoBox = Box<dyn TypeInfoImplementation + Sync + Send>;

pub(crate) type TypeRegistry = HashMap<String, TypeInfoBox>;

pub(crate) fn get_type_registry() -> TypeRegistry {
    chain!(
        arithmetic::types().into_iter(),
        gas_station::types().into_iter(),
        tuple_obj::types().into_iter(),
    )
    .collect()
}

pub(crate) fn get_info(registry: &TypeRegistry, ty: &Type) -> Result<TypeInfo, Error> {
    match registry.get(&ty.name) {
        None => Err(Error::UnsupportedTypeName(ty.name.clone())),
        Some(e) => e.get_info(&ty.args, registry),
    }
}
