use cairo_lang_defs::diagnostic_utils::StableLocation;
use cairo_lang_diagnostics::{Maybe, ToMaybe};
use cairo_lang_semantic as semantic;
use cairo_lang_utils::ordered_hash_map::OrderedHashMap;
use cairo_lang_utils::ordered_hash_set::OrderedHashSet;
use itertools::chain;

use super::context::{LoweredExpr, LoweringContext, LoweringFlowError, LoweringResult, VarRequest};
use crate::{
    BlockId, RefIndex, Statement, StructuredBlock, StructuredBlockEnd, StructuredStatement,
    VarRemapping, VariableId,
};

// TODO(yg): remove pubs?
/// StructuredBlock builder, describing its current state.
pub struct BlockBuilder {
    /// The variable ids currently bound to the ref variables.
    current_refs: Vec<Option<VariableId>>,
    /// The variable ids bound to the ref variables (including implicits) at the beginning of the
    /// block.
    pub initial_refs: Option<Vec<VariableId>>,
    /// Variables given as inputs to the block, including implicits. Relevant for function blocks /
    /// match arm blocks, etc...
    inputs: Vec<VariableId>,
    /// A store for implicit variables, owning their OwnedVariable instances.
    implicits: OrderedHashMap<semantic::TypeId, VariableId>,
    // The implicits that are changed in this block.
    changed_implicits: OrderedHashSet<semantic::TypeId>,
    /// A store for semantic variables, owning their OwnedVariable instances.
    semantics: OrderedHashMap<semantic::VarId, VariableId>,
    // The semantic variables that are added/changed in this block.
    changed_semantics: OrderedHashSet<semantic::VarId>,
    /// Current sequence of lowered statements emitted.
    statements: Vec<StructuredStatement>,
    /// Statement pending finalize_statement().
    pending_statement: Option<Statement>,
    /// Updates to the variable ids bound to the ref variables (including implicits), from the last
    /// update until exactly after next statement. When finalize_statement() will be called, these
    /// updates will be added to the statement.
    pending_ref_updates: OrderedHashMap<RefIndex, VariableId>,
    /// The id to use for this block when it's finalized.
    pub pending_block: BlockId,
}
impl BlockBuilder {
    /// Creates a new [BlockBuilder] for the root block of a function body.
    pub fn root(ctx: &LoweringContext<'_>, block_id: BlockId) -> Self {
        BlockBuilder {
            current_refs: (0..(ctx.implicits.len() + ctx.ref_params.len())).map(|_| None).collect(),
            initial_refs: None,
            inputs: vec![],
            implicits: Default::default(),
            changed_implicits: Default::default(),
            semantics: Default::default(),
            changed_semantics: Default::default(),
            statements: Default::default(),
            pending_statement: None,
            pending_ref_updates: Default::default(),
            pending_block: block_id,
        }
    }

    /// Creates a [BlockBuilder] for a subscope.
    pub fn subscope(&self, block_id: BlockId) -> BlockBuilder {
        BlockBuilder {
            current_refs: self.current_refs.clone(),
            initial_refs: None,
            inputs: vec![],
            implicits: self.implicits.clone(),
            changed_implicits: Default::default(),
            semantics: self.semantics.clone(),
            changed_semantics: Default::default(),
            statements: Default::default(),
            pending_statement: None,
            pending_ref_updates: Default::default(),
            pending_block: block_id,
        }
    }

    /// Creates a [BlockBuilder] for a subscope with unchanged refs.
    pub fn subscope_with_bound_refs(&self, block_id: BlockId) -> BlockBuilder {
        let mut subscope = self.subscope(block_id);
        subscope.bind_refs();
        subscope
    }

    pub fn sibling_scope(&self, block_id: BlockId) -> BlockBuilder {
        // TODO(yg): should chnaged implicits/semantics be nonempty? Can this even work?
        let mut scope = BlockBuilder {
            current_refs: self.current_refs.clone(),
            initial_refs: None,
            inputs: self.inputs.clone(),
            implicits: self.implicits.clone(),
            changed_implicits: Default::default(),
            semantics: self.semantics.clone(),
            changed_semantics: Default::default(),
            statements: Default::default(),
            pending_statement: None,
            pending_ref_updates: Default::default(),
            pending_block: block_id,
        };
        scope.bind_refs();
        scope
    }

    /// Adds an input to the block.
    pub fn add_input(&mut self, ctx: &mut LoweringContext<'_>, req: VarRequest) -> VariableId {
        let var_id = ctx.new_var(req);
        self.inputs.push(var_id);
        var_id
    }

    /// Binds an implicit to a lowered variable.
    pub fn put_implicit(
        &mut self,
        ctx: &mut LoweringContext<'_>,
        ty: semantic::TypeId,
        var: VariableId,
    ) {
        if self.implicits.insert(ty, var) == Some(var) {
            return;
        }
        self.changed_implicits.insert(ty);
        let (implicit_index, _) = ctx
            .implicits
            .iter()
            .enumerate()
            .find(|(_, imp_ty)| **imp_ty == ty)
            .expect("Unknown implicit.");
        self.pending_ref_updates.insert(RefIndex(implicit_index), var);
        self.current_refs[implicit_index] = Some(var);
    }

    /// Gets the current lowered variable bound to an implicit.
    pub fn get_implicit(&mut self, ty: semantic::TypeId) -> VariableId {
        self.implicits.get(&ty).copied().expect("Use of undefined implicit cannot happen.")
    }

    /// Binds a semantic variable to a lowered variable.
    pub fn put_semantic(
        &mut self,
        ctx: &mut LoweringContext<'_>,
        semantic_var_id: semantic::VarId,
        var: VariableId,
    ) {
        if self.semantics.insert(semantic_var_id, var) == Some(var) {
            return;
        }
        self.changed_semantics.insert(semantic_var_id);
        if let Some((ref_index, _)) = ctx
            .ref_params
            .iter()
            .enumerate()
            .find(|(_, ref_semantic_var_id)| **ref_semantic_var_id == semantic_var_id)
        {
            let index = ctx.implicits.len() + ref_index;
            self.pending_ref_updates.insert(RefIndex(index), var);
            self.current_refs[index] = Some(var);
        }
    }

    /// Gets the current lowered variable bound to a semantic variable.
    pub fn get_semantic(&mut self, semantic_var_id: semantic::VarId) -> VariableId {
        self.semantics
            .get(&semantic_var_id)
            .copied()
            .expect("Use of undefined variable cannot happen after semantic phase.")
    }

    /// Confirms that all refs (including implicits) for the beginning of the block are set using
    /// put_implicit() and put_semantic(). Should be called once per block before any statement.
    pub fn bind_refs(&mut self) {
        assert!(self.initial_refs.is_none(), "References cannot be bound twice.");
        self.initial_refs = Some(
            self.current_refs.iter().copied().map(|v| v.expect("Reference not bound.")).collect(),
        );
    }

    /// Adds a statement to the block. finalize_statement() should be called after binding the refs
    /// that are relevant right after the statement, and always before the next push_statement().
    pub fn push_statement(&mut self, stmt: Statement) {
        assert!(self.pending_statement.replace(stmt).is_none(), "finalize_statement() not called.");
    }

    /// Finalizes a statement after binding its refs.
    pub fn finalize_statement(&mut self) {
        let statement = self.pending_statement.take().expect("push_statement() not called.");
        self.statements.push(StructuredStatement {
            statement,
            ref_updates: std::mem::take(&mut self.pending_ref_updates),
        });
    }

    /// Adds a statement to the block without rebinding refs - the refs will remain like they were
    /// before this statement.
    pub fn push_finalized_statement(&mut self, stmt: Statement) {
        self.push_statement(stmt);
        self.finalize_statement();
    }

    /// Ends a block with Unreachable.
    pub fn unreachable(self, ctx: &mut LoweringContext<'_>) -> StructuredBlock {
        self.finalize(ctx, StructuredBlockEnd::Unreachable)
    }

    /// Ends a block with Panic.
    pub fn panic(self, ctx: &mut LoweringContext<'_>, data: VariableId) -> Maybe<StructuredBlock> {
        let implicit_vars = ctx
            .implicits
            .iter()
            .map(|ty| self.implicits.get(ty).copied())
            .collect::<Option<Vec<_>>>()
            .to_maybe()?;

        let ref_vars = ctx
            .ref_params
            .iter()
            .map(|semantic_var_id| self.semantics.get(semantic_var_id).copied())
            .collect::<Option<Vec<_>>>()
            .to_maybe()?;

        Ok(self.finalize(
            ctx,
            StructuredBlockEnd::Panic { refs: chain!(implicit_vars, ref_vars).collect(), data },
        ))
    }

    /// Ends a block with Callsite.
    pub fn goto_callsite(self, expr: Option<VariableId>) -> SealedBlockBuilder {
        SealedBlockBuilder::GotoCallsite { scope: self, expr }
    }

    /// Ends a block with Return.
    pub fn ret(self, ctx: &mut LoweringContext<'_>, expr: VariableId) -> Maybe<StructuredBlock> {
        let implicit_vars = ctx
            .implicits
            .iter()
            .map(|ty| self.implicits.get(ty).copied())
            .collect::<Option<Vec<_>>>()
            .to_maybe()?;

        let ref_vars = ctx
            .ref_params
            .iter()
            .map(|semantic_var_id| self.semantics.get(semantic_var_id).copied())
            .collect::<Option<Vec<_>>>()
            .to_maybe()?;

        Ok(self.finalize(
            ctx,
            StructuredBlockEnd::Return {
                refs: chain!(implicit_vars, ref_vars).collect(),
                returns: vec![expr],
            },
        ))
    }

    /// Ends a block with known ending information. Used by [SealedBlockBuilder].
    pub fn finalize(
        self,
        ctx: &mut LoweringContext<'_>,
        end: StructuredBlockEnd,
    ) -> StructuredBlock {
        let block = StructuredBlock {
            initial_refs: self.initial_refs.expect("References have not been bound yet."),
            inputs: self.inputs,
            statements: self.statements,
            end,
            id: self.pending_block,
        };
        // ctx.blocks.set_block(block.id, block.clone());
        block
    }

    pub fn goto(&mut self, ctx: &mut LoweringContext<'_>, target: BlockId) {
        let block = StructuredBlock {
            initial_refs: self.initial_refs.clone().expect("References have not been bound yet."),
            inputs: self.inputs.clone(),
            statements: self.statements.clone(),
            end: StructuredBlockEnd::Goto { target, remapping: VarRemapping::default() },
            id: self.pending_block,
        };
        ctx.blocks.set_block(block.id, block);
        *self = self.sibling_scope(target);
    }
}

/// Remapping of lowered variables with more semantic information regarding what is the semantic
/// role of the lowered variables.
#[derive(Debug, Default)]
pub struct SemanticRemapping {
    expr: Option<VariableId>,
    implicits: OrderedHashMap<semantic::TypeId, VariableId>,
    semantics: OrderedHashMap<semantic::VarId, VariableId>,
}

/// A sealed BlockBuilder, ready to be merged with sibling blocks to end the block.
#[allow(clippy::large_enum_variant)]
pub enum SealedBlockBuilder {
    /// Block should end by goto callsite. `expr` may be None for blocks that return the unit type.
    GotoCallsite { scope: BlockBuilder, expr: Option<VariableId> },
    /// Block end is already known.
    Ends(StructuredBlock),
}
impl SealedBlockBuilder {
    // TODO(Yg): update doc.
    /// Given the extra information needed, returns the ID of the final StructuredBlock.
    fn finalize(
        self,
        ctx: &mut LoweringContext<'_>,
        semantic_remapping: &SemanticRemapping,
    ) -> BlockId {
        println!("yg finalizing sealed block");
        let block = match self {
            SealedBlockBuilder::GotoCallsite { scope, expr } => {
                let mut remapping = VarRemapping::default();
                // Since SemanticRemapping should have unique variable ids, these asserts will pass.
                for (ty, remapped_var) in semantic_remapping.implicits.iter() {
                    assert!(remapping.insert(*remapped_var, scope.implicits[*ty]).is_none());
                }
                for (semantic, remapped_var) in semantic_remapping.semantics.iter() {
                    assert!(remapping.insert(*remapped_var, scope.semantics[*semantic]).is_none());
                }
                if let Some(remapped_var) = semantic_remapping.expr {
                    let expr = expr.expect("Block expr is unit, while sibling block isn't.");
                    assert!(remapping.insert(remapped_var, expr).is_none());
                }

                scope.finalize(ctx, StructuredBlockEnd::Callsite(remapping))
            }
            SealedBlockBuilder::Ends(block) => block,
        };
        let block_id = block.id;
        println!("yg finalizing sealed block id: {}", block_id.0);
        ctx.blocks.set_block(block_id, block);
        block_id
    }
}
impl From<StructuredBlock> for SealedBlockBuilder {
    fn from(b: StructuredBlock) -> Self {
        SealedBlockBuilder::Ends(b)
    }
}

/// Information regarding the merge of sibling blocks.
pub struct MergedBlocks {
    /// The converged expression of the blocks, usable at the calling scope.
    pub expr: LoweringResult<LoweredExpr>,
    /// Merged block ids.
    pub blocks: Vec<BlockId>,
    pub following_block: Option<BlockId>,
}

/// Merges sibling sealed blocks.
pub fn merge_sealed(
    ctx: &mut LoweringContext<'_>,
    scope: &mut BlockBuilder,
    sealed_blocks: Vec<SealedBlockBuilder>,
    location: StableLocation,
) -> MergedBlocks {
    // TODO(spapini): When adding Gotos, include the callsite target in the required information to
    // merge.
    // TODO(spapini): Don't remap if we have a single reachable branch.

    let mut semantic_remapping = SemanticRemapping::default();
    let mut n_reachable_blocks = 0;

    // Remap Variables from all blocks.
    for sealed_block in &sealed_blocks {
        let SealedBlockBuilder::GotoCallsite { scope: subscope, expr } = sealed_block else {
            continue;
        };
        n_reachable_blocks += 1;
        if let Some(var) = expr {
            semantic_remapping.expr.get_or_insert_with(|| {
                let var = ctx.variables[*var].clone();
                ctx.variables.alloc(var)
            });
        }
        for semantic in subscope.changed_semantics.iter() {
            if !scope.semantics.contains_key(semantic) {
                // This variable is local to the subscope.
                continue;
            }
            // This variable belongs to an outer scope, and it is changed in at least one branch.
            // It should be remapped.
            semantic_remapping.semantics.entry(*semantic).or_insert_with(|| {
                let var = ctx.variables[subscope.semantics[*semantic]].clone();
                ctx.variables.alloc(var)
            });
        }
        for implicit in subscope.changed_implicits.iter() {
            semantic_remapping.implicits.entry(*implicit).or_insert_with(|| {
                let var = ctx.variables[subscope.implicits[*implicit]].clone();
                ctx.variables.alloc(var)
            });
        }
    }

    // If there are reachable blocks, create a new empty block for the code after this match.
    let following_block = if n_reachable_blocks == 0 {
        None
    } else {
        Some(ctx.blocks.alloc_empty(|id| StructuredBlock::new(id)))
    };

    let blocks = sealed_blocks.into_iter().map(|s| s.finalize(ctx, &semantic_remapping)).collect();

    // Apply remapping on scope.
    for (implicit, var) in semantic_remapping.implicits {
        scope.put_implicit(ctx, implicit, var);
    }
    for (semantic, var) in semantic_remapping.semantics {
        scope.put_semantic(ctx, semantic, var);
    }

    let expr = match semantic_remapping.expr {
        _ if n_reachable_blocks == 0 => Err(LoweringFlowError::Unreachable),
        Some(var) => Ok(LoweredExpr::AtVariable(var)),
        None => Ok(LoweredExpr::Tuple { exprs: vec![], location }),
    };
    MergedBlocks { expr, blocks, following_block }
}
