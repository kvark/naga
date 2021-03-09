/*! Module analyzer.

Figures out the following properties:
  - control flow uniformity
  - texture/sampler pairs
  - expression reference counts
!*/

use crate::arena::{Arena, Handle};
use std::ops;

/// Uniform control flow characteristics.
#[derive(Clone, Debug, Default)]
#[cfg_attr(feature = "serialize", derive(serde::Serialize))]
#[cfg_attr(feature = "deserialize", derive(serde::Deserialize))]
#[cfg_attr(test, derive(PartialEq))]
pub struct Uniformity {
    /// A child expression with non-uniform result.
    ///
    /// This means, when the relevant invocations are scheduled on a compute unit,
    /// they have to use vector registers to store an individual value
    /// per invocation.
    ///
    /// Whenever the control flow is conditioned on such value,
    /// the hardware needs to keep track of the mask of invocations,
    /// and process all branches of the control flow.
    ///
    /// Any operations that depend on non-uniform results also produce non-uniform.
    non_uniform_result: Option<Handle<crate::Expression>>,
    /// A child expression that requires uniform control flow.
    ///
    /// Some operations can only be done within uniform control flow:
    /// derivatives and auto-level image sampling in fragment shaders,
    /// and group barriers in compute shaders.
    require_uniform: Option<Handle<crate::Expression>>,
}

//TODO: instead of doing cur | next, we could reverse this everywhere
// and do `next | cur`, which would allow us to trace the cause of
// uniformity requirement/disruption across the expression chain.

impl ops::BitOr for Uniformity {
    type Output = Self;
    fn bitor(self, other: Self) -> Self {
        Uniformity {
            non_uniform_result: self.non_uniform_result.or(other.non_uniform_result),
            require_uniform: self.require_uniform.or(other.require_uniform),
        }
    }
}

impl ops::BitOrAssign for Uniformity {
    fn bitor_assign(&mut self, other: Self) {
        *self = self.clone() | other;
    }
}

impl Uniformity {
    fn non_uniform_result(expr: Handle<crate::Expression>) -> Self {
        Uniformity {
            non_uniform_result: Some(expr),
            require_uniform: None,
        }
    }

    fn require_uniform(expr: Handle<crate::Expression>) -> Self {
        Uniformity {
            non_uniform_result: None,
            require_uniform: Some(expr),
        }
    }

    fn disruptor(&self) -> Option<UniformityDisruptor> {
        self.non_uniform_result.map(UniformityDisruptor::Expression)
    }
}

bitflags::bitflags! {
    struct ExitFlags: u8 {
        /// Control flow may return from the function, which makes all the
        /// subsequent statements within the current function (only!)
        /// to be executed in a non-uniform control flow.
        const MAY_RETURN = 0x1;
        /// Control flow may be killed. Anything after `Statement::Kill` is
        /// considered inside non-uniform context.
        const MAY_KILL = 0x2;
    }
}

bitflags::bitflags! {
    /// Indicates how a global variable is used.
    #[cfg_attr(feature = "serialize", derive(serde::Serialize))]
    #[cfg_attr(feature = "deserialize", derive(serde::Deserialize))]
    pub struct GlobalUse: u8 {
        /// Data will be read from the variable.
        const READ = 0x1;
        /// Data will be written to the variable.
        const WRITE = 0x2;
        /// The information about the data is queried.
        const QUERY = 0x4;
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
#[cfg_attr(feature = "serialize", derive(serde::Serialize))]
#[cfg_attr(feature = "deserialize", derive(serde::Deserialize))]
pub struct SamplingKey {
    pub image: Handle<crate::GlobalVariable>,
    pub sampler: Handle<crate::GlobalVariable>,
}

#[derive(Clone, Default)]
#[cfg_attr(feature = "serialize", derive(serde::Serialize))]
#[cfg_attr(feature = "deserialize", derive(serde::Deserialize))]
pub struct ExpressionInfo {
    pub uniformity: Uniformity,
    pub ref_count: usize,
    assignable_global: Option<Handle<crate::GlobalVariable>>,
}

#[cfg_attr(feature = "serialize", derive(serde::Serialize))]
#[cfg_attr(feature = "deserialize", derive(serde::Deserialize))]
pub struct FunctionInfo {
    /// Uniformity characteristics.
    pub uniformity: Uniformity,
    /// Function may kill the invocation.
    pub may_kill: bool,
    /// Set of image-sampler pais used with sampling.
    pub sampling_set: crate::FastHashSet<SamplingKey>,
    /// Vector of global variable usages.
    ///
    /// Each item corresponds to a global variable in the module.
    global_uses: Box<[GlobalUse]>,
    /// Vector of expression infos.
    ///
    /// Each item corresponds to an expression in the function.
    expressions: Box<[ExpressionInfo]>,
}

impl FunctionInfo {
    pub fn global_variable_count(&self) -> usize {
        self.global_uses.len()
    }
    pub fn expression_count(&self) -> usize {
        self.expressions.len()
    }
    pub fn dominates_global_use(&self, other: &Self) -> bool {
        for (self_global_uses, other_global_uses) in
            self.global_uses.iter().zip(other.global_uses.iter())
        {
            if !self_global_uses.contains(*other_global_uses) {
                return false;
            }
        }
        true
    }
}

impl ops::Index<Handle<crate::GlobalVariable>> for FunctionInfo {
    type Output = GlobalUse;
    fn index(&self, handle: Handle<crate::GlobalVariable>) -> &GlobalUse {
        &self.global_uses[handle.index()]
    }
}

impl ops::Index<Handle<crate::Expression>> for FunctionInfo {
    type Output = ExpressionInfo;
    fn index(&self, handle: Handle<crate::Expression>) -> &ExpressionInfo {
        &self.expressions[handle.index()]
    }
}

/// Disruptor of the uniform control flow.
#[derive(Clone, Copy, Debug, thiserror::Error)]
#[cfg_attr(test, derive(PartialEq))]
pub enum UniformityDisruptor {
    #[error("Expression {0:?} produced non-uniform result, and control flow depends on it")]
    Expression(Handle<crate::Expression>),
    #[error("There is a Return earlier in the control flow of the function")]
    Return,
    #[error("There is a Kill earlier in the entry point across all called functions")]
    Kill,
}

impl UniformityDisruptor {
    fn from_exit(flags: ExitFlags) -> Option<Self> {
        if flags.contains(ExitFlags::MAY_RETURN) {
            Some(Self::Return)
        } else if flags.contains(ExitFlags::MAY_KILL) {
            Some(Self::Kill)
        } else {
            None
        }
    }
}

#[derive(Clone, Debug, thiserror::Error)]
#[cfg_attr(test, derive(PartialEq))]
pub enum AnalysisError {
    #[error("Expression {0:?} is not a global variable!")]
    ExpectedGlobalVariable(crate::Expression),
    #[error("Required uniformity of control flow for {0:?} is not fulfilled because of {1:?}")]
    NonUniformControlFlow(Handle<crate::Expression>, UniformityDisruptor),
}

impl FunctionInfo {
    /// Adds a value-type reference to an expression.
    #[must_use]
    fn add_ref_impl(
        &mut self,
        handle: Handle<crate::Expression>,
        global_use: GlobalUse,
    ) -> Uniformity {
        let info = &mut self.expressions[handle.index()];
        info.ref_count += 1;
        // mark the used global as read
        if let Some(global) = info.assignable_global {
            self.global_uses[global.index()] |= global_use;
        }
        info.uniformity.clone()
    }

    /// Adds a value-type reference to an expression.
    #[must_use]
    fn add_ref(&mut self, handle: Handle<crate::Expression>) -> Uniformity {
        self.add_ref_impl(handle, GlobalUse::READ)
    }

    /// Adds a potentially assignable reference to an expression.
    /// These are destinations for `Store` and `ImageStore` statements,
    /// which can transit through `Access` and `AccessIndex`.
    #[must_use]
    fn add_assignable_ref(
        &mut self,
        handle: Handle<crate::Expression>,
        assignable_global: &mut Option<Handle<crate::GlobalVariable>>,
    ) -> Uniformity {
        let info = &mut self.expressions[handle.index()];
        info.ref_count += 1;
        // propagate the assignable global up the chain, till it either hits
        // a value-type expression, or the assignment statement.
        if let Some(global) = info.assignable_global {
            if let Some(_old) = assignable_global.replace(global) {
                unreachable!()
            }
        }
        info.uniformity.clone()
    }

    /// Inherit information from a called function.
    fn process_call(&mut self, info: &Self) -> Uniformity {
        for key in info.sampling_set.iter() {
            self.sampling_set.insert(key.clone());
        }
        for (mine, other) in self.global_uses.iter_mut().zip(info.global_uses.iter()) {
            *mine |= *other;
        }
        info.uniformity.clone()
    }

    /// Computes the control flags of a given expression, and store them
    /// in `self.expressions`. Also, bumps the reference counts on
    /// dependent expressions.
    fn process_expression(
        &mut self,
        handle: Handle<crate::Expression>,
        expression_arena: &Arena<crate::Expression>,
        global_var_arena: &Arena<crate::GlobalVariable>,
        other_functions: &[FunctionInfo],
    ) -> Result<(), AnalysisError> {
        use crate::{Expression as E, SampleLevel as Sl};

        let mut assignable_global = None;
        let uniformity = match expression_arena[handle] {
            E::Access { base, index } => {
                self.add_assignable_ref(base, &mut assignable_global) | self.add_ref(index)
            }
            E::AccessIndex { base, .. } => self.add_assignable_ref(base, &mut assignable_global),
            E::Constant(_) => Uniformity::default(),
            E::Compose { ref components, .. } => {
                let mut accum = Uniformity::default();
                for &comp in components {
                    accum |= self.add_ref(comp);
                }
                accum
            }
            E::FunctionArgument(_) => Uniformity::non_uniform_result(handle), //TODO?
            E::GlobalVariable(gh) => {
                assignable_global = Some(gh);
                let var = &global_var_arena[gh];
                let uniform = if let Some(crate::Binding::BuiltIn(built_in)) = var.binding {
                    match built_in {
                        // per-polygon built-ins are uniform
                        crate::BuiltIn::FrontFacing
                        // per-work-group built-ins are uniform
                        | crate::BuiltIn::WorkGroupId
                        | crate::BuiltIn::WorkGroupSize => true,
                        _ => false,
                    }
                } else {
                    use crate::StorageClass as Sc;
                    match var.class {
                        // only flat inputs are uniform
                        Sc::Input => var.interpolation == Some(crate::Interpolation::Flat),
                        Sc::Output | Sc::Function | Sc::Private | Sc::WorkGroup => false,
                        // uniform data
                        Sc::Uniform | Sc::PushConstant => true,
                        // storage data is only uniform when read-only
                        Sc::Handle | Sc::Storage => {
                            !var.storage_access.contains(crate::StorageAccess::STORE)
                        }
                    }
                };
                if uniform {
                    Uniformity::default()
                } else {
                    Uniformity::non_uniform_result(handle)
                }
            }
            E::LocalVariable(_) => {
                Uniformity::non_uniform_result(handle) //TODO?
            }
            E::Load { pointer } => self.add_ref(pointer),
            E::ImageSample {
                image,
                sampler,
                coordinate,
                array_index,
                offset: _,
                level,
                depth_ref,
            } => {
                self.sampling_set.insert(SamplingKey {
                    image: match expression_arena[image] {
                        crate::Expression::GlobalVariable(var) => var,
                        ref other => {
                            return Err(AnalysisError::ExpectedGlobalVariable(other.clone()))
                        }
                    },
                    sampler: match expression_arena[sampler] {
                        crate::Expression::GlobalVariable(var) => var,
                        ref other => {
                            return Err(AnalysisError::ExpectedGlobalVariable(other.clone()))
                        }
                    },
                });
                let array_flags = match array_index {
                    Some(h) => self.add_ref(h),
                    None => Uniformity::default(),
                };
                let level_flags = match level {
                    // implicit derivatives for LOD require uniform
                    Sl::Auto => Uniformity::require_uniform(handle),
                    Sl::Zero => Uniformity::default(),
                    Sl::Exact(h) | Sl::Bias(h) => self.add_ref(h),
                    Sl::Gradient { x, y } => self.add_ref(x) | self.add_ref(y),
                };
                let dref_flags = match depth_ref {
                    Some(h) => self.add_ref(h),
                    None => Uniformity::default(),
                };
                self.add_ref(image)
                    | self.add_ref(sampler)
                    | self.add_ref(coordinate)
                    | array_flags
                    | level_flags
                    | dref_flags
            }
            E::ImageLoad {
                image,
                coordinate,
                array_index,
                index,
            } => {
                let array_flags = match array_index {
                    Some(h) => self.add_ref(h),
                    None => Uniformity::default(),
                };
                let index_flags = match index {
                    Some(h) => self.add_ref(h),
                    None => Uniformity::default(),
                };
                self.add_ref(image) | self.add_ref(coordinate) | array_flags | index_flags
            }
            E::ImageQuery { image, query } => {
                let query_flags = match query {
                    crate::ImageQuery::Size { level: Some(h) } => self.add_ref(h),
                    _ => Uniformity::default(),
                };
                self.add_ref_impl(image, GlobalUse::QUERY) | query_flags
            }
            E::Unary { expr, .. } => self.add_ref(expr),
            E::Binary { left, right, .. } => self.add_ref(left) | self.add_ref(right),
            E::Select {
                condition,
                accept,
                reject,
            } => self.add_ref(condition) | self.add_ref(accept) | self.add_ref(reject),
            // explicit derivatives require uniform
            E::Derivative { expr, .. } => Uniformity::require_uniform(handle) | self.add_ref(expr),
            E::Relational { argument, .. } => self.add_ref(argument),
            E::Math {
                arg, arg1, arg2, ..
            } => {
                let arg1_flags = match arg1 {
                    Some(h) => self.add_ref(h),
                    None => Uniformity::default(),
                };
                let arg2_flags = match arg2 {
                    Some(h) => self.add_ref(h),
                    None => Uniformity::default(),
                };
                self.add_ref(arg) | arg1_flags | arg2_flags
            }
            E::As { expr, .. } => self.add_ref(expr),
            E::Call(function) => self.process_call(&other_functions[function.index()]),
            E::ArrayLength(expr) => self.add_ref_impl(expr, GlobalUse::QUERY),
        };

        self.expressions[handle.index()] = ExpressionInfo {
            uniformity,
            ref_count: 0,
            assignable_global,
        };
        Ok(())
    }

    /// Computes the control flags on the block (as a sequence of statements),
    /// and returns them. The parent control flow is uniform if `is_uniform` is true.
    ///
    /// Returns a `NonUniformControlFlow` error if any of the expressions in the block
    /// require uniformity, but the current flow is non-uniform.
    #[allow(clippy::or_fun_call)]
    fn process_block(
        &mut self,
        statements: &[crate::Statement],
        other_functions: &[FunctionInfo],
        mut disruptor: Option<UniformityDisruptor>,
    ) -> Result<(Uniformity, ExitFlags), AnalysisError> {
        use crate::Statement as S;
        let mut block_uniformity = Uniformity::default();
        let mut block_exit = ExitFlags::empty();
        for statement in statements {
            let (cur_uniformity, cur_exit) = match *statement {
                S::Emit(_) | S::Break | S::Continue => (Uniformity::default(), ExitFlags::empty()),
                S::Kill => (Uniformity::default(), ExitFlags::MAY_KILL),
                S::Block(ref b) => self.process_block(b, other_functions, disruptor)?,
                S::If {
                    condition,
                    ref accept,
                    ref reject,
                } => {
                    let condition_uniformity = self.add_ref(condition);
                    if let (Some(expr), Some(cause)) =
                        (condition_uniformity.require_uniform, disruptor)
                    {
                        return Err(AnalysisError::NonUniformControlFlow(expr, cause));
                    }
                    let branch_disruptor = disruptor.or(condition_uniformity.disruptor());
                    let (accept_uniformity, accept_exit) =
                        self.process_block(accept, other_functions, branch_disruptor)?;
                    let (reject_uniformity, reject_exit) =
                        self.process_block(reject, other_functions, branch_disruptor)?;
                    (
                        condition_uniformity | accept_uniformity | reject_uniformity,
                        accept_exit | reject_exit,
                    )
                }
                S::Switch {
                    selector,
                    ref cases,
                    ref default,
                } => {
                    let mut uniformity = self.add_ref(selector);
                    let mut exit = ExitFlags::empty();
                    let branch_disruptor = disruptor.or(uniformity.disruptor());
                    let mut case_disruptor = disruptor;
                    for case in cases.iter() {
                        let (case_uniformity, case_exit) =
                            self.process_block(&case.body, other_functions, case_disruptor)?;
                        uniformity |= case_uniformity;
                        exit |= case_exit;
                        case_disruptor = if case.fall_through {
                            case_disruptor.or(UniformityDisruptor::from_exit(case_exit))
                        } else {
                            branch_disruptor
                        };
                    }
                    let (default_uniformity, default_exit) =
                        self.process_block(default, other_functions, branch_disruptor)?;
                    (uniformity | default_uniformity, exit | default_exit)
                }
                S::Loop {
                    ref body,
                    ref continuing,
                } => {
                    let (body_uniformity, body_exit) =
                        self.process_block(body, other_functions, disruptor)?;
                    let branch_disruptor = disruptor.or(UniformityDisruptor::from_exit(body_exit));
                    let (continuing_uniformity, continuing_exit) =
                        self.process_block(continuing, other_functions, branch_disruptor)?;
                    (
                        body_uniformity | continuing_uniformity,
                        body_exit | continuing_exit,
                    )
                }
                S::Return { value } => {
                    let uniformity = match value {
                        Some(expr) => self.add_ref(expr),
                        None => Uniformity::default(),
                    };
                    //TODO: if we are in the uniform control flow, should this still be an exit flag?
                    (uniformity, ExitFlags::MAY_RETURN)
                }
                S::Store { pointer, value } => {
                    let uniformity =
                        self.add_ref_impl(pointer, GlobalUse::WRITE) | self.add_ref(value);
                    (uniformity, ExitFlags::empty())
                }
                S::ImageStore {
                    image,
                    coordinate,
                    array_index,
                    value,
                } => {
                    let array_uniformity = match array_index {
                        Some(expr) => self.add_ref(expr),
                        None => Uniformity::default(),
                    };
                    let uniformity = array_uniformity
                        | self.add_ref_impl(image, GlobalUse::WRITE)
                        | self.add_ref(coordinate)
                        | self.add_ref(value);
                    (uniformity, ExitFlags::empty())
                }
                S::Call {
                    function,
                    ref arguments,
                    result,
                } => {
                    let info = &other_functions[function.index()];
                    let mut uniformity = self.process_call(info);
                    for &argument in arguments {
                        uniformity |= self.add_ref(argument);
                    }
                    if let Some(expr) = result {
                        uniformity |= self.add_ref(expr);
                    }
                    let exit = if info.may_kill {
                        ExitFlags::MAY_KILL
                    } else {
                        ExitFlags::empty()
                    };
                    (uniformity, exit)
                }
            };

            if let (Some(expr), Some(cause)) = (cur_uniformity.require_uniform, disruptor) {
                return Err(AnalysisError::NonUniformControlFlow(expr, cause));
            }
            disruptor = disruptor.or(UniformityDisruptor::from_exit(cur_exit));
            block_uniformity |= cur_uniformity;
            block_exit |= cur_exit;
        }
        Ok((block_uniformity, block_exit))
    }
}

#[derive(Default)]
#[cfg_attr(feature = "serialize", derive(serde::Serialize))]
#[cfg_attr(feature = "deserialize", derive(serde::Deserialize))]
pub struct Analysis {
    functions: Vec<FunctionInfo>,
    entry_points: Vec<FunctionInfo>,
}

impl Analysis {
    /// Builds the `FunctionInfo` based on the function, and validates the
    /// uniform control flow if required by the expressions of this function.
    fn process_function(
        &self,
        fun: &crate::Function,
        global_var_arena: &Arena<crate::GlobalVariable>,
    ) -> Result<FunctionInfo, AnalysisError> {
        let mut info = FunctionInfo {
            uniformity: Uniformity::default(),
            may_kill: false,
            sampling_set: crate::FastHashSet::default(),
            global_uses: vec![GlobalUse::empty(); global_var_arena.len()].into_boxed_slice(),
            expressions: vec![ExpressionInfo::default(); fun.expressions.len()].into_boxed_slice(),
        };

        for (handle, _) in fun.expressions.iter() {
            info.process_expression(handle, &fun.expressions, global_var_arena, &self.functions)?;
        }

        let (uniformity, exit) = info.process_block(&fun.body, &self.functions, None)?;
        info.uniformity = uniformity;
        info.may_kill = exit.contains(ExitFlags::MAY_KILL);

        Ok(info)
    }

    /// Analyze a module and return the `Analysis`, if successful.
    pub fn new(module: &crate::Module) -> Result<Self, AnalysisError> {
        let mut this = Analysis {
            functions: Vec::with_capacity(module.functions.len()),
            entry_points: Vec::with_capacity(module.entry_points.len()),
        };
        for (_, fun) in module.functions.iter() {
            let info = this.process_function(fun, &module.global_variables)?;
            this.functions.push(info);
        }

        for ep in module.entry_points.iter() {
            let info = this.process_function(&ep.function, &module.global_variables)?;
            this.entry_points.push(info);
        }

        Ok(this)
    }

    pub fn get_entry_point(&self, index: usize) -> &FunctionInfo {
        &self.entry_points[index]
    }
}

impl ops::Index<Handle<crate::Function>> for Analysis {
    type Output = FunctionInfo;
    fn index(&self, handle: Handle<crate::Function>) -> &FunctionInfo {
        &self.functions[handle.index()]
    }
}

#[test]
fn uniform_control_flow() {
    use crate::{Expression as E, Statement as S};

    let mut constant_arena = Arena::new();
    let constant = constant_arena.append(crate::Constant {
        name: None,
        specialization: None,
        inner: crate::ConstantInner::Scalar {
            width: 4,
            value: crate::ScalarValue::Uint(0),
        },
    });
    let mut type_arena = Arena::new();
    let ty = type_arena.append(crate::Type {
        name: None,
        inner: crate::TypeInner::Scalar {
            kind: crate::ScalarKind::Float,
            width: 4,
        },
    });
    let mut global_var_arena = Arena::new();
    let non_uniform_global = global_var_arena.append(crate::GlobalVariable {
        name: None,
        init: None,
        ty,
        binding: Some(crate::Binding::BuiltIn(crate::BuiltIn::VertexIndex)),
        class: crate::StorageClass::Input,
        interpolation: None,
        storage_access: crate::StorageAccess::empty(),
    });
    let uniform_global = global_var_arena.append(crate::GlobalVariable {
        name: None,
        init: None,
        ty,
        binding: Some(crate::Binding::Location(0)),
        class: crate::StorageClass::Input,
        interpolation: Some(crate::Interpolation::Flat),
        storage_access: crate::StorageAccess::empty(),
    });

    let mut expressions = Arena::new();
    // checks the uniform control flow
    let constant_expr = expressions.append(E::Constant(constant));
    // checks the non-uniform control flow
    let derivative_expr = expressions.append(E::Derivative {
        axis: crate::DerivativeAxis::X,
        expr: constant_expr,
    });
    let non_uniform_global_expr = expressions.append(E::GlobalVariable(non_uniform_global));
    let uniform_global_expr = expressions.append(E::GlobalVariable(uniform_global));
    // checks the QUERY flag
    let query_expr = expressions.append(E::ArrayLength(uniform_global_expr));
    // checks the transitive WRITE flag
    let access_expr = expressions.append(E::AccessIndex {
        base: non_uniform_global_expr,
        index: 1,
    });

    let mut info = FunctionInfo {
        uniformity: Uniformity::default(),
        may_kill: false,
        sampling_set: crate::FastHashSet::default(),
        global_uses: vec![GlobalUse::empty(); global_var_arena.len()].into_boxed_slice(),
        expressions: vec![ExpressionInfo::default(); expressions.len()].into_boxed_slice(),
    };
    for (handle, _) in expressions.iter() {
        info.process_expression(handle, &expressions, &global_var_arena, &[])
            .unwrap();
    }
    assert_eq!(info[non_uniform_global_expr].ref_count, 1);
    assert_eq!(info[uniform_global_expr].ref_count, 1);
    assert_eq!(info[query_expr].ref_count, 0);
    assert_eq!(info[access_expr].ref_count, 0);
    assert_eq!(info[non_uniform_global], GlobalUse::empty());
    assert_eq!(info[uniform_global], GlobalUse::QUERY);

    let stmt_if_uniform = S::If {
        condition: uniform_global_expr,
        accept: Vec::new(),
        reject: vec![S::Store {
            pointer: constant_expr,
            value: derivative_expr,
        }],
    };
    assert_eq!(
        info.process_block(&[stmt_if_uniform], &[], None),
        Ok((
            Uniformity::require_uniform(derivative_expr),
            ExitFlags::empty()
        )),
    );
    assert_eq!(info[constant_expr].ref_count, 2);
    assert_eq!(info[uniform_global], GlobalUse::READ | GlobalUse::QUERY);

    let stmt_if_non_uniform = S::If {
        condition: non_uniform_global_expr,
        accept: vec![S::Store {
            pointer: constant_expr,
            value: derivative_expr,
        }],
        reject: Vec::new(),
    };
    assert_eq!(
        info.process_block(&[stmt_if_non_uniform], &[], None),
        Err(AnalysisError::NonUniformControlFlow(
            derivative_expr,
            UniformityDisruptor::Expression(non_uniform_global_expr)
        )),
    );
    assert_eq!(info[derivative_expr].ref_count, 2);
    assert_eq!(info[non_uniform_global], GlobalUse::READ);

    let stmt_return_non_uniform = S::Return {
        value: Some(non_uniform_global_expr),
    };
    assert_eq!(
        info.process_block(
            &[stmt_return_non_uniform],
            &[],
            Some(UniformityDisruptor::Return)
        ),
        Ok((
            Uniformity::non_uniform_result(non_uniform_global_expr),
            ExitFlags::MAY_RETURN
        )),
    );
    assert_eq!(info[non_uniform_global_expr].ref_count, 3);

    let stmt_assign = S::Store {
        pointer: access_expr,
        value: query_expr,
    };
    assert_eq!(
        info.process_block(&[stmt_assign], &[], Some(UniformityDisruptor::Kill)),
        Ok((
            Uniformity::non_uniform_result(non_uniform_global_expr),
            ExitFlags::empty()
        )),
    );
    assert_eq!(info[non_uniform_global], GlobalUse::READ | GlobalUse::WRITE);
}
