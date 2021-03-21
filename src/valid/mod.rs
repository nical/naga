mod analyzer;
mod expression;
mod function;
mod interface;
mod r#type;

use crate::{
    arena::{Arena, Handle},
    proc::{Layouter, Typifier},
    FastHashSet,
};
use bit_set::BitSet;

//TODO: analyze the model at the same time as we validate it,
// merge the corresponding matches over expressions and statements.
pub use analyzer::{
    AnalysisError, ExpressionInfo, FunctionInfo, GlobalUse, ModuleInfo, Uniformity,
    UniformityRequirements,
};
pub use expression::ExpressionError;
pub use function::{CallError, FunctionError, LocalVariableError};
pub use interface::{EntryPointError, GlobalVariableError, VaryingError};
pub use r#type::{Disalignment, TypeError, TypeFlags};

bitflags::bitflags! {
    /// Validation flags.
    #[cfg_attr(feature = "serialize", derive(serde::Serialize))]
    #[cfg_attr(feature = "deserialize", derive(serde::Deserialize))]
    pub struct ValidationFlags: u8 {
        const EXPRESSIONS = 0x1;
        const BLOCKS = 0x2;
        const CONTROL_FLOW_UNIFORMITY = 0x4;
    }
}

#[derive(Debug)]
pub struct Validator {
    flags: ValidationFlags,
    //Note: this is a bit tricky: some of the front-ends as well as backends
    // already have to use the typifier, so the work here is redundant in a way.
    typifier: Typifier,
    types: Vec<r#type::TypeInfo>,
    location_mask: BitSet,
    bind_group_masks: Vec<BitSet>,
    select_cases: FastHashSet<i32>,
    valid_expression_list: Vec<Handle<crate::Expression>>,
    valid_expression_set: BitSet,
}

#[derive(Clone, Debug, thiserror::Error)]
pub enum ConstantError {
    #[error("The type doesn't match the constant")]
    InvalidType,
    #[error("The component handle {0:?} can not be resolved")]
    UnresolvedComponent(Handle<crate::Constant>),
    #[error("The array size handle {0:?} can not be resolved")]
    UnresolvedSize(Handle<crate::Constant>),
}

#[derive(Clone, Debug, thiserror::Error)]
pub enum ValidationError {
    #[error("Type {handle:?} '{name}' is invalid")]
    Type {
        handle: Handle<crate::Type>,
        name: String,
        #[source]
        error: TypeError,
    },
    #[error("Constant {handle:?} '{name}' is invalid")]
    Constant {
        handle: Handle<crate::Constant>,
        name: String,
        #[source]
        error: ConstantError,
    },
    #[error("Global variable {handle:?} '{name}' is invalid")]
    GlobalVariable {
        handle: Handle<crate::GlobalVariable>,
        name: String,
        #[source]
        error: GlobalVariableError,
    },
    #[error("Function {handle:?} '{name}' is invalid")]
    Function {
        handle: Handle<crate::Function>,
        name: String,
        #[source]
        error: FunctionError,
    },
    #[error("Entry point {name} at {stage:?} is invalid")]
    EntryPoint {
        stage: crate::ShaderStage,
        name: String,
        #[source]
        error: EntryPointError,
    },
    #[error(transparent)]
    Analysis(#[from] AnalysisError),
    #[error("Module is corrupted")]
    Corrupted,
}

impl crate::TypeInner {
    fn is_sized(&self) -> bool {
        match *self {
            Self::Scalar { .. }
            | Self::Vector { .. }
            | Self::Matrix { .. }
            | Self::Array {
                size: crate::ArraySize::Constant(_),
                ..
            }
            | Self::Pointer { .. }
            | Self::ValuePointer { .. }
            | Self::Struct { .. } => true,
            Self::Array { .. } | Self::Image { .. } | Self::Sampler { .. } => false,
        }
    }
}

impl Validator {
    /// Construct a new validator instance.
    pub fn new(flags: ValidationFlags) -> Self {
        Validator {
            flags,
            typifier: Typifier::new(),
            types: Vec::new(),
            location_mask: BitSet::new(),
            bind_group_masks: Vec::new(),
            select_cases: FastHashSet::default(),
            valid_expression_list: Vec::new(),
            valid_expression_set: BitSet::new(),
        }
    }

    fn validate_constant(
        &self,
        handle: Handle<crate::Constant>,
        constants: &Arena<crate::Constant>,
        types: &Arena<crate::Type>,
    ) -> Result<(), ConstantError> {
        let con = &constants[handle];
        match con.inner {
            crate::ConstantInner::Scalar { width, ref value } => {
                if !Self::check_width(value.scalar_kind(), width) {
                    return Err(ConstantError::InvalidType);
                }
            }
            crate::ConstantInner::Composite { ty, ref components } => {
                match types[ty].inner {
                    crate::TypeInner::Array {
                        size: crate::ArraySize::Dynamic,
                        ..
                    } => {
                        return Err(ConstantError::InvalidType);
                    }
                    crate::TypeInner::Array {
                        size: crate::ArraySize::Constant(size_handle),
                        ..
                    } => {
                        if handle <= size_handle {
                            return Err(ConstantError::UnresolvedSize(size_handle));
                        }
                    }
                    _ => {} //TODO
                }
                if let Some(&comp) = components.iter().find(|&&comp| handle <= comp) {
                    return Err(ConstantError::UnresolvedComponent(comp));
                }
            }
        }
        Ok(())
    }

    /// Check the given module to be valid.
    pub fn validate(&mut self, module: &crate::Module) -> Result<ModuleInfo, ValidationError> {
        self.reset_types(module.types.len());

        let mod_info = ModuleInfo::new(module, self.flags)?;

        let layouter = Layouter::new(&module.types, &module.constants);

        for (handle, constant) in module.constants.iter() {
            self.validate_constant(handle, &module.constants, &module.types)
                .map_err(|error| ValidationError::Constant {
                    handle,
                    name: constant.name.clone().unwrap_or_default(),
                    error,
                })?;
        }

        // doing after the globals, so that `type_flags` is ready
        for (handle, ty) in module.types.iter() {
            let ty_info = self
                .validate_type(ty, handle, &module.constants, &layouter)
                .map_err(|error| ValidationError::Type {
                    handle,
                    name: ty.name.clone().unwrap_or_default(),
                    error,
                })?;
            self.types[handle.index()] = ty_info;
        }

        for (var_handle, var) in module.global_variables.iter() {
            self.validate_global_var(var, &module.types)
                .map_err(|error| ValidationError::GlobalVariable {
                    handle: var_handle,
                    name: var.name.clone().unwrap_or_default(),
                    error,
                })?;
        }

        for (handle, fun) in module.functions.iter() {
            self.validate_function(fun, &mod_info[handle], module)
                .map_err(|error| ValidationError::Function {
                    handle,
                    name: fun.name.clone().unwrap_or_default(),
                    error,
                })?;
        }

        let mut ep_map = FastHashSet::default();
        for (index, ep) in module.entry_points.iter().enumerate() {
            if !ep_map.insert((ep.stage, &ep.name)) {
                return Err(ValidationError::EntryPoint {
                    stage: ep.stage,
                    name: ep.name.clone(),
                    error: EntryPointError::Conflict,
                });
            }
            let info = mod_info.get_entry_point(index);
            self.validate_entry_point(ep, info, module)
                .map_err(|error| ValidationError::EntryPoint {
                    stage: ep.stage,
                    name: ep.name.clone(),
                    error,
                })?;
        }

        Ok(mod_info)
    }
}