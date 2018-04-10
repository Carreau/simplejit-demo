use std::collections::HashMap;

use cretonne::entity::EntityRef;
use cretonne::ir::{AbiParam, InstBuilder, Value, Ebb, Signature, CallConv};
use cretonne::ir::types;
use cretonne::ir::condcodes::IntCC;
use cretonne;
use cton_frontend::{FunctionBuilderContext, FunctionBuilder, Variable};
use cton_module::{Module, Linkage};
use cton_simplejit::SimpleJITBackend;

/// The AST node for expressions.
pub enum Expr {
    Literal(String),
    Identifier(String),
    Assign(String, Box<Expr>),
    Eq(Box<Expr>, Box<Expr>),
    Ne(Box<Expr>, Box<Expr>),
    Lt(Box<Expr>, Box<Expr>),
    Le(Box<Expr>, Box<Expr>),
    Gt(Box<Expr>, Box<Expr>),
    Ge(Box<Expr>, Box<Expr>),
    Add(Box<Expr>, Box<Expr>),
    Sub(Box<Expr>, Box<Expr>),
    Mul(Box<Expr>, Box<Expr>),
    Div(Box<Expr>, Box<Expr>),
    IfElse(Box<Expr>, Vec<Expr>, Vec<Expr>),
    WhileLoop(Box<Expr>, Vec<Expr>),
    Call(String, Vec<Expr>),
}

/// Include the parser code, generated from grammar.rustpeg.
mod parser {
    include!(concat!(env!("OUT_DIR"), "/grammar.rs"));
}

/// The basic JIT class.
pub struct JIT {
    /// The function builder context, which is reused across multiple
    /// FunctionBuilder instances.
    builder_context: FunctionBuilderContext<Variable>,

    /// The main Cretonne context, which holds the state for codegen.
    ctx: cretonne::Context,

    /// The module, with the simplejit backend, which manages the JIT'd
    /// functions.
    module: Module<SimpleJITBackend>,
}

impl JIT {
    /// Create a new `JIT` instance.
    pub fn new() -> Self {
        let backend = SimpleJITBackend::new();
        Self {
            builder_context: FunctionBuilderContext::<Variable>::new(),
            ctx: cretonne::Context::new(),
            module: Module::new(backend),
        }
    }

    /// Compile a string in the toy language into machine code.
    pub fn compile(&mut self, input: &str) -> Result<*const u8, String> {
        // First, parse the string, producing AST nodes.
        let (name, params, the_return, stmts) =
            parser::function(&input).map_err(|e| e.to_string())?;

        // Then, translate the AST nodes into Cretonne IR.
        self.translate(params, the_return, stmts).map_err(
            |e| e.to_string(),
        )?;

        // Next, declare the function to simplejit. Functions must be declared
        // before they can be called, or defined.
        //
        // TODO: This may be an area where the API should be streamlined; should
        // we have a version of `declare_function` that automatically declares
        // the function?
        let id = self.module
            .declare_function(&name, Linkage::Export, &self.ctx.func.signature)
            .map_err(|e| e.to_string())?;

        // Define the function to simplejit. This finishes compilation, although
        // there may be outstanding relocations to perform. Currently, simplejit
        // cannot finish relocations until all functions to be called are
        // defined. For this toy demo for now, we'll just finalize the function
        // below.
        self.module.define_function(id, &mut self.ctx).map_err(
            |e| {
                e.to_string()
            },
        )?;

        // Now that compilation is finished, we can clear out the context state.
        self.ctx.clear();

        // Finalize the function, finishing any outstanding relocations. The
        // result is a pointer to the finished machine code.
        let code = self.module.finalize_function(id);

        Ok(code)
    }

    // Translate from toy-language AST nodes into Cretonne IR.
    fn translate(
        &mut self,
        params: Vec<String>,
        the_return: String,
        stmts: Vec<Expr>,
    ) -> Result<(), String> {
        // Our toy language currently only supports I32 values, though Cretonne
        // supports other types.
        for _p in &params {
            self.ctx.func.signature.params.push(
                AbiParam::new(types::I32),
            );
        }

        // Our toy language currently only supports one return value, though
        // Cretonne is designed to support more.
        self.ctx.func.signature.returns.push(
            AbiParam::new(types::I32),
        );

        // Create the builder to builder a function.
        let mut builder =
            FunctionBuilder::<Variable>::new(&mut self.ctx.func, &mut self.builder_context);

        // Create the entry block, to start emitting code in.
        let entry_ebb = builder.create_ebb();

        // Since this is the entry block, add block parameters corresponding to
        // the function's parameters.
        //
        // TODO: Streamline the API here.
        builder.append_ebb_params_for_function_params(entry_ebb);

        // Tell the builder to emit code in this block.
        builder.switch_to_block(entry_ebb);

        // And, tell the builder that this block will have no further
        // predecessors. Since it's the entry block, it won't have any
        // predecessors.
        builder.seal_block(entry_ebb);

        // The toy language allows variables to be declared implicitly.
        // Walk the AST and declare all implicitly-declared variables.
        let variables = declare_variables(&mut builder, &params, &the_return, &stmts, entry_ebb);

        // Now translate the statements of the function body.
        let mut trans = FunctionTranslator {
            builder,
            variables,
            module: &mut self.module,
        };
        for expr in stmts {
            trans.translate_expr(expr);
        }

        // Set up the return variable of the function. Above, we declared a
        // variable to hold the return value. Here, we just do a use of that
        // variable.
        let return_variable = trans.variables.get(&the_return).unwrap();
        let return_value = trans.builder.use_var(*return_variable);

        // Emit the return instruction.
        trans.builder.ins().return_(&[return_value]);

        // Tell the builder we're done with this function.
        trans.builder.finalize();
        Ok(())
    }
}

/// A collection of state used for translating from toy-language AST nodes
/// into Cretonne IR.
struct FunctionTranslator<'a> {
    builder: FunctionBuilder<'a, Variable>,
    variables: HashMap<String, Variable>,
    module: &'a mut Module<SimpleJITBackend>,
}

impl<'a> FunctionTranslator<'a> {
    /// When you write out instructions in Cretonne, you get back `Value`s. You
    /// can then use these references in other instructions.
    fn translate_expr(&mut self, expr: Expr) -> Value {
        match expr {
            Expr::Literal(literal) => {
                let imm: i32 = literal.parse().unwrap();
                self.builder.ins().iconst(types::I32, i64::from(imm))
            }

            Expr::Add(lhs, rhs) => {
                let lhs = self.translate_expr(*lhs);
                let rhs = self.translate_expr(*rhs);
                self.builder.ins().iadd(lhs, rhs)
            }

            Expr::Sub(lhs, rhs) => {
                let lhs = self.translate_expr(*lhs);
                let rhs = self.translate_expr(*rhs);
                self.builder.ins().isub(lhs, rhs)
            }

            Expr::Mul(lhs, rhs) => {
                let lhs = self.translate_expr(*lhs);
                let rhs = self.translate_expr(*rhs);
                self.builder.ins().imul(lhs, rhs)
            }

            Expr::Div(lhs, rhs) => {
                let lhs = self.translate_expr(*lhs);
                let rhs = self.translate_expr(*rhs);
                self.builder.ins().udiv(lhs, rhs)
            }

            Expr::Eq(lhs, rhs) => {
                let lhs = self.translate_expr(*lhs);
                let rhs = self.translate_expr(*rhs);
                let c = self.builder.ins().icmp(IntCC::Equal, lhs, rhs);
                self.builder.ins().bint(types::I32, c)
            }

            Expr::Ne(lhs, rhs) => {
                let lhs = self.translate_expr(*lhs);
                let rhs = self.translate_expr(*rhs);
                let c = self.builder.ins().icmp(IntCC::NotEqual, lhs, rhs);
                self.builder.ins().bint(types::I32, c)
            }

            Expr::Lt(lhs, rhs) => {
                let lhs = self.translate_expr(*lhs);
                let rhs = self.translate_expr(*rhs);
                let c = self.builder.ins().icmp(IntCC::SignedLessThan, lhs, rhs);
                self.builder.ins().bint(types::I32, c)
            }

            Expr::Le(lhs, rhs) => {
                let lhs = self.translate_expr(*lhs);
                let rhs = self.translate_expr(*rhs);
                let c = self.builder.ins().icmp(
                    IntCC::SignedLessThanOrEqual,
                    lhs,
                    rhs,
                );
                self.builder.ins().bint(types::I32, c)
            }

            Expr::Gt(lhs, rhs) => {
                let lhs = self.translate_expr(*lhs);
                let rhs = self.translate_expr(*rhs);
                let c = self.builder.ins().icmp(IntCC::SignedGreaterThan, lhs, rhs);
                self.builder.ins().bint(types::I32, c)
            }

            Expr::Ge(lhs, rhs) => {
                let lhs = self.translate_expr(*lhs);
                let rhs = self.translate_expr(*rhs);
                let c = self.builder.ins().icmp(
                    IntCC::SignedGreaterThanOrEqual,
                    lhs,
                    rhs,
                );
                self.builder.ins().bint(types::I32, c)
            }

            Expr::Call(name, args) => self.translate_call(name, args),

            Expr::Identifier(name) => {
                // `use_var` is used to read the value of a variable.
                let variable = self.variables.get(&name).unwrap();
                self.builder.use_var(*variable)
            }

            Expr::Assign(name, expr) => {
                // `def_var` is used to write the value of a variable. Note that
                // variables can have multiple definitions. Cretonne will
                // convert them into SSA form for itself automatically.
                let new_value = self.translate_expr(*expr);
                let variable = self.variables.get(&name).unwrap();
                self.builder.def_var(*variable, new_value);
                new_value
            }

            Expr::IfElse(condition, then_body, else_body) => {
                let condition_value = self.translate_expr(*condition);

                let else_block = self.builder.create_ebb();
                let merge_block = self.builder.create_ebb();

                // If-else constructs in the toy language have a return value.
                // In traditional SSA form, this would produce a PHI between
                // the then and else bodies. Cretonne uses block parameters,
                // so set up a parameter in the merge block, and we'll pass
                // the return values to it from the branches.
                self.builder.append_ebb_param(merge_block, types::I32);

                // Test the if condition and conditionally branch.
                self.builder.ins().brz(condition_value, else_block, &[]);

                let mut then_return = self.builder.ins().iconst(types::I32, 0);
                for expr in then_body {
                    then_return = self.translate_expr(expr);
                }

                // Jump to the merge block, passing it the block return value.
                self.builder.ins().jump(merge_block, &[then_return]);

                self.builder.switch_to_block(else_block);
                self.builder.seal_block(else_block);
                let mut else_return = self.builder.ins().iconst(types::I32, 0);
                for expr in else_body {
                    else_return = self.translate_expr(expr);
                }

                // Jump to the merge block, passing it the block return value.
                self.builder.ins().jump(merge_block, &[else_return]);

                // Switch to the merge block for subsequent statements.
                self.builder.switch_to_block(merge_block);

                // We've now seen all the predecessors of the merge block.
                self.builder.seal_block(merge_block);

                // Read the value of the if-else by reading the merge block
                // parameter.
                let phi = self.builder.ebb_params(merge_block)[0];

                phi
            }

            Expr::WhileLoop(condition, loop_body) => {
                let header_block = self.builder.create_ebb();
                let exit_block = self.builder.create_ebb();
                self.builder.ins().jump(header_block, &[]);
                self.builder.switch_to_block(header_block);

                let condition_value = self.translate_expr(*condition);
                self.builder.ins().brz(condition_value, exit_block, &[]);

                for expr in loop_body {
                    self.translate_expr(expr);
                }
                self.builder.ins().jump(header_block, &[]);

                self.builder.switch_to_block(exit_block);

                // We've reached the bottom of the loop, so there will be no
                // more backedges to the header to exits to the bottom.
                self.builder.seal_block(header_block);
                self.builder.seal_block(exit_block);

                // Just return 0 for now.
                self.builder.ins().iconst(types::I32, 0)
            }
        }
    }

    fn translate_call(&mut self, name: String, args: Vec<Expr>) -> Value {
        let mut sig = Signature::new(CallConv::SystemV);

        // Add a parameter for each argument.
        for _arg in &args {
            sig.params.push(AbiParam::new(types::I32));
        }

        // For simplicity for now, just make all calls return a single I32.
        sig.returns.push(AbiParam::new(types::I32));

        // TODO: Streamline the API here?
        let callee = self.module
            .declare_function(&name, Linkage::Export, &sig)
            .expect("problem declaring function");
        let local_callee = self.module.declare_func_in_func(
            callee,
            &mut self.builder.func,
        );

        let mut arg_values = Vec::new();
        for arg in args {
            arg_values.push(self.translate_expr(arg))
        }
        let call = self.builder.ins().call(local_callee, &arg_values);
        self.builder.inst_results(call)[0]
    }
}

fn declare_variables(
    builder: &mut FunctionBuilder<Variable>,
    params: &[String],
    the_return: &str,
    stmts: &[Expr],
    entry_ebb: Ebb,
) -> HashMap<String, Variable> {
    let mut variables = HashMap::new();
    let mut index = 0;

    for (i, name) in params.iter().enumerate() {
        // TODO: cton_frontend should really have an API to make it easy to set
        // up param variables.
        let val = builder.ebb_params(entry_ebb)[i];
        let var = declare_variable(builder, &mut variables, &mut index, name);
        builder.def_var(var, val);
    }
    let zero = builder.ins().iconst(types::I32, 0);
    let return_variable = declare_variable(builder, &mut variables, &mut index, the_return);
    builder.def_var(return_variable, zero);
    for expr in stmts {
        declare_variables_in_stmt(builder, &mut variables, &mut index, expr);
    }

    variables
}

/// Recursively descend through the AST, translating all implicit
/// variable declarations.
fn declare_variables_in_stmt(
    builder: &mut FunctionBuilder<Variable>,
    variables: &mut HashMap<String, Variable>,
    index: &mut usize,
    expr: &Expr,
) {
    match *expr {
        Expr::Assign(ref name, _) => {
            declare_variable(builder, variables, index, name);
        }
        Expr::IfElse(ref _condition, ref then_body, ref else_body) => {
            for stmt in then_body {
                declare_variables_in_stmt(builder, variables, index, &stmt);
            }
            for stmt in else_body {
                declare_variables_in_stmt(builder, variables, index, &stmt);
            }
        }
        Expr::WhileLoop(ref _condition, ref loop_body) => {
            for stmt in loop_body {
                declare_variables_in_stmt(builder, variables, index, &stmt);
            }
        }
        _ => (),
    }
}

fn declare_variable(
    builder: &mut FunctionBuilder<Variable>,
    variables: &mut HashMap<String, Variable>,
    index: &mut usize,
    name: &str,
) -> Variable {
    let var = Variable::new(*index);
    if !variables.contains_key(name) {
        variables.insert(name.into(), var);
        builder.declare_var(var, types::I32);
        *index += 1;
    }
    var
}
