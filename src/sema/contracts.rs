use crate::parser::pt;
use inkwell::OptimizationLevel;
use num_bigint::BigInt;
use num_traits::Zero;
use std::collections::HashMap;
use std::collections::HashSet;
use std::convert::TryInto;
use tiny_keccak::{Hasher, Keccak};

use super::expression::match_constructor_to_args;
use super::functions;
use super::statements;
use super::symtable::Symtable;
use super::variables;
use super::{ast, SOLANA_FIRST_OFFSET};
use crate::{emit, Target};

impl ast::Contract {
    /// Create a new contract, abstract contract, interface or library
    pub fn new(name: &str, ty: pt::ContractTy, tags: Vec<ast::Tag>, loc: pt::Loc) -> Self {
        ast::Contract {
            name: name.to_owned(),
            loc,
            ty,
            bases: Vec::new(),
            using: Vec::new(),
            layout: Vec::new(),
            fixed_layout_size: BigInt::zero(),
            dynamic_storage: false,
            tags,
            functions: Vec::new(),
            all_functions: HashMap::new(),
            virtual_functions: HashMap::new(),
            variables: Vec::new(),
            creates: Vec::new(),
            sends_events: Vec::new(),
            initializer: None,
            default_constructor: None,
            cfg: Vec::new(),
        }
    }

    /// Generate contract code for this contract
    pub fn emit<'a>(
        &'a self,
        ns: &'a ast::Namespace,
        context: &'a inkwell::context::Context,
        filename: &'a str,
        opt: OptimizationLevel,
        math_overflow_check: bool,
    ) -> emit::Binary {
        emit::Binary::build(context, self, ns, filename, opt, math_overflow_check)
    }

    /// Print the entire contract; storage initializers, constructors and functions and their CFGs
    pub fn print_cfg(&self, ns: &ast::Namespace) -> String {
        let mut out = format!("#\n# Contract: {}\n#\n\n", self.name);

        for cfg in &self.cfg {
            if !cfg.is_placeholder() {
                out += &format!(
                    "\n# {} {} public:{} selector:{} nonpayable:{}\n",
                    cfg.ty,
                    cfg.name,
                    cfg.public,
                    hex::encode(cfg.selector.to_be_bytes()),
                    cfg.nonpayable,
                );

                out += &format!(
                    "# params: {}\n",
                    cfg.params
                        .iter()
                        .map(|p| p.ty.to_string(ns))
                        .collect::<Vec<String>>()
                        .join(",")
                );
                out += &format!(
                    "# returns: {}\n",
                    cfg.returns
                        .iter()
                        .map(|p| p.ty.to_string(ns))
                        .collect::<Vec<String>>()
                        .join(",")
                );

                out += &cfg.to_string(self, ns);
            }
        }

        out
    }

    /// Selector for this contract. This is used by Solana contract bundle
    pub fn selector(&self) -> u32 {
        let mut hasher = Keccak::v256();
        let mut hash = [0u8; 32];
        hasher.update(self.name.as_bytes());
        hasher.finalize(&mut hash);

        u32::from_le_bytes(hash[0..4].try_into().unwrap())
    }
}

/// Resolve the following contract
pub fn resolve(
    contracts: &[(usize, &pt::ContractDefinition)],
    file_no: usize,
    ns: &mut ast::Namespace,
) {
    resolve_base_contracts(contracts, file_no, ns);

    resolve_using(contracts, file_no, ns);

    // we need to resolve declarations first, so we call functions/constructors of
    // contracts before they are declared
    let mut function_bodies = Vec::new();

    for (contract_no, def) in contracts {
        function_bodies.extend(resolve_declarations(def, file_no, *contract_no, ns));
    }

    // Resolve base contract constructor arguments on contract definition (not constructor definitions)
    resolve_base_args(contracts, file_no, ns);

    // Now we have all the declarations, we can create the layout of storage and handle base contracts
    for (contract_no, _) in contracts {
        layout_contract(*contract_no, ns);
    }

    // Now we can resolve the bodies
    if !resolve_bodies(function_bodies, file_no, ns) {
        // only if we could resolve all the bodies
        for (contract_no, _) in contracts {
            check_base_args(*contract_no, ns);
        }
    }
}

/// Resolve the base contracts list and check for cycles. Returns true if no
/// issues where found.
fn resolve_base_contracts(
    contracts: &[(usize, &pt::ContractDefinition)],
    file_no: usize,
    ns: &mut ast::Namespace,
) {
    for (contract_no, def) in contracts {
        for base in &def.base {
            if ns.contracts[*contract_no].is_library() {
                ns.diagnostics.push(ast::Diagnostic::error(
                    base.loc,
                    format!(
                        "library ‘{}’ cannot have a base contract",
                        ns.contracts[*contract_no].name
                    ),
                ));
                continue;
            }
            let name = &base.name;
            match ns.resolve_contract(file_no, name) {
                Some(no) => {
                    if no == *contract_no {
                        ns.diagnostics.push(ast::Diagnostic::error(
                            name.loc,
                            format!(
                                "contract ‘{}’ cannot have itself as a base contract",
                                name.name
                            ),
                        ));
                    } else if ns.contracts[*contract_no]
                        .bases
                        .iter()
                        .any(|e| e.contract_no == no)
                    {
                        ns.diagnostics.push(ast::Diagnostic::error(
                            name.loc,
                            format!(
                                "contract ‘{}’ duplicate base ‘{}’",
                                ns.contracts[*contract_no].name, name.name
                            ),
                        ));
                    } else if is_base(*contract_no, no, ns) {
                        ns.diagnostics.push(ast::Diagnostic::error(
                            name.loc,
                            format!(
                                "base ‘{}’ from contract ‘{}’ is cyclic",
                                name.name, ns.contracts[*contract_no].name
                            ),
                        ));
                    } else if ns.contracts[*contract_no].is_interface()
                        && !ns.contracts[no].is_interface()
                    {
                        ns.diagnostics.push(ast::Diagnostic::error(
                            name.loc,
                            format!(
                                "interface ‘{}’ cannot have {} ‘{}’ as a base",
                                ns.contracts[*contract_no].name, ns.contracts[no].ty, name.name
                            ),
                        ));
                    } else if ns.contracts[no].is_library() {
                        let contract = &ns.contracts[*contract_no];

                        ns.diagnostics.push(ast::Diagnostic::error(
                            name.loc,
                            format!(
                                "library ‘{}’ cannot be used as base contract for {} ‘{}’",
                                name.name, contract.ty, contract.name,
                            ),
                        ));
                    } else {
                        // We do not resolve the constructor arguments here, since we have not
                        // resolved any variables. This means no constants can be used on base
                        // constructor args, so we delay this until resolve_base_args()
                        ns.contracts[*contract_no].bases.push(ast::Base {
                            loc: base.loc,
                            contract_no: no,
                            constructor: None,
                        });
                    }
                }
                None => {
                    ns.diagnostics.push(ast::Diagnostic::error(
                        name.loc,
                        format!("contract ‘{}’ not found", name.name),
                    ));
                }
            }
        }
    }
}

/// Resolve the base contracts list and check for cycles. Returns true if no
/// issues where found.
fn resolve_base_args(
    contracts: &[(usize, &pt::ContractDefinition)],
    file_no: usize,
    ns: &mut ast::Namespace,
) {
    let mut diagnostics = Vec::new();

    // for every contract, if we have a base which resolved successfully, resolve any constructor args
    for (contract_no, def) in contracts {
        for base in &def.base {
            let name = &base.name;
            if let Some(base_no) = ns.resolve_contract(file_no, name) {
                if let Some(pos) = ns.contracts[*contract_no]
                    .bases
                    .iter()
                    .position(|e| e.contract_no == base_no)
                {
                    if let Some(args) = &base.args {
                        let symtable = Symtable::new();

                        // find constructor which matches this
                        if let Ok((Some(constructor_no), args)) = match_constructor_to_args(
                            &base.loc,
                            args,
                            file_no,
                            base_no,
                            *contract_no,
                            ns,
                            &symtable,
                            &mut diagnostics,
                        ) {
                            ns.contracts[*contract_no].bases[pos].constructor =
                                Some((constructor_no, args));
                        }
                    }
                }
            }
        }
    }

    ns.diagnostics.extend(diagnostics);
}

/// Visit base contracts in depth-first post-order
pub fn visit_bases(contract_no: usize, ns: &ast::Namespace) -> Vec<usize> {
    let mut order = Vec::new();

    fn base(contract_no: usize, order: &mut Vec<usize>, ns: &ast::Namespace) {
        for b in ns.contracts[contract_no].bases.iter().rev() {
            base(b.contract_no, order, ns);
        }

        if !order.contains(&contract_no) {
            order.push(contract_no);
        }
    }

    base(contract_no, &mut order, ns);

    order
}

// Is a contract a base of another contract
pub fn is_base(base: usize, parent: usize, ns: &ast::Namespace) -> bool {
    let bases = &ns.contracts[parent].bases;

    if base == parent || bases.iter().any(|e| e.contract_no == base) {
        return true;
    }

    bases
        .iter()
        .any(|parent| is_base(base, parent.contract_no, ns))
}

/// Layout the contract. We determine the layout of variables and deal with overriding variables
fn layout_contract(contract_no: usize, ns: &mut ast::Namespace) {
    let mut function_syms: HashMap<String, ast::Symbol> = HashMap::new();
    let mut variable_syms: HashMap<String, ast::Symbol> = HashMap::new();
    let mut override_needed: HashMap<String, Vec<(usize, usize)>> = HashMap::new();

    let mut slot = if ns.target == Target::Solana {
        BigInt::from(SOLANA_FIRST_OFFSET)
    } else {
        BigInt::zero()
    };

    for base_contract_no in visit_bases(contract_no, ns) {
        // find file number where contract is defined
        let contract_file_no = ns.contracts[base_contract_no].loc.0;

        // find all syms for this contract
        for ((file_no, iter_contract_no, name), sym) in
            ns.variable_symbols.iter().chain(ns.function_symbols.iter())
        {
            if *iter_contract_no != Some(base_contract_no) || *file_no != contract_file_no {
                continue;
            }

            let mut done = false;

            if let Some(ast::Symbol::Function(ref mut list)) = function_syms.get_mut(name) {
                if let ast::Symbol::Function(funcs) = sym {
                    list.extend(funcs.to_owned());
                    done = true;
                }
            }

            if !done {
                if let Some(prev) = variable_syms.get(name).or_else(|| function_syms.get(name)) {
                    // events can be redefined, so allow duplicate event symbols
                    // if a variable has an accessor function (i.e. public) then allow the variable sym,
                    // check for duplicates will be on accessor function
                    if !(prev.has_accessor(ns)
                        || sym.has_accessor(ns)
                        || prev.is_event() && sym.is_event())
                    {
                        ns.diagnostics.push(ast::Diagnostic::error_with_note(
                            *sym.loc(),
                            format!("already defined ‘{}’", name),
                            *prev.loc(),
                            format!("previous definition of ‘{}’", name),
                        ));
                    }
                }
            }

            if !sym.is_private_variable(ns) {
                if let ast::Symbol::Function(_) = sym {
                    function_syms.insert(name.to_owned(), sym.clone());
                } else {
                    variable_syms.insert(name.to_owned(), sym.clone());
                }
            }
        }

        for var_no in 0..ns.contracts[base_contract_no].variables.len() {
            if !ns.contracts[base_contract_no].variables[var_no].constant {
                let ty = ns.contracts[base_contract_no].variables[var_no].ty.clone();

                if ns.target == Target::Solana {
                    // elements need to be aligned on solana
                    let alignment = ty.align_of(ns);

                    let offset = slot.clone() % alignment;

                    if offset > BigInt::zero() {
                        slot += alignment - offset;
                    }
                }

                ns.contracts[contract_no].layout.push(ast::Layout {
                    slot: slot.clone(),
                    contract_no: base_contract_no,
                    var_no,
                    ty: ty.clone(),
                });

                if ty.is_dynamic(ns) {
                    ns.contracts[contract_no].dynamic_storage = true;
                }

                slot += ty.storage_slots(ns);
            }
        }

        // add functions to our function_table
        for function_no in ns.contracts[base_contract_no].functions.clone() {
            let cur = &ns.functions[function_no];

            let signature = cur.signature.to_owned();

            if let Some(entry) = override_needed.get(&signature) {
                let non_virtual = entry
                    .iter()
                    .filter_map(|(_, function_no)| {
                        let func = &ns.functions[*function_no];

                        if func.is_virtual {
                            None
                        } else {
                            Some(ast::Note {
                                pos: func.loc,
                                message: format!(
                                    "function ‘{}’ is not specified ‘virtual’",
                                    func.name
                                ),
                            })
                        }
                    })
                    .collect::<Vec<ast::Note>>();

                if !non_virtual.is_empty() {
                    ns.diagnostics.push(ast::Diagnostic::error_with_notes(
                        cur.loc,
                        format!(
                            "function ‘{}’ overrides functions which are not ‘virtual’",
                            cur.name
                        ),
                        non_virtual,
                    ));
                }

                let source_override = entry
                    .iter()
                    .map(|(contract_no, _)| -> &str { &ns.contracts[*contract_no].name })
                    .collect::<Vec<&str>>()
                    .join(",");

                if let Some((loc, override_specified)) = &cur.is_override {
                    if override_specified.is_empty() && entry.len() > 1 {
                        ns.diagnostics.push(ast::Diagnostic::error(
                            *loc,
                            format!(
                                "function ‘{}’ should specify override list ‘override({})’",
                                cur.name, source_override
                            ),
                        ));
                    } else {
                        let override_specified: HashSet<usize> =
                            override_specified.iter().cloned().collect();
                        let override_needed: HashSet<usize> =
                            entry.iter().map(|(contract_no, _)| *contract_no).collect();

                        // List of contract which should have been specified
                        let missing: Vec<String> = override_needed
                            .difference(&override_specified)
                            .map(|contract_no| ns.contracts[*contract_no].name.to_owned())
                            .collect();

                        if !missing.is_empty() && override_needed.len() >= 2 {
                            ns.diagnostics.push(ast::Diagnostic::error(
                                *loc,
                                format!(
                                    "function ‘{}’ missing overrides ‘{}’, specify ‘override({})’",
                                    cur.name,
                                    missing.join(","),
                                    source_override
                                ),
                            ));
                        }

                        // List of contract which should not have been specified
                        let extra: Vec<String> = override_specified
                            .difference(&override_needed)
                            .map(|contract_no| ns.contracts[*contract_no].name.to_owned())
                            .collect();

                        if !extra.is_empty() {
                            ns.diagnostics.push(ast::Diagnostic::error(
                                *loc,
                                format!(
                                    "function ‘{}’ includes extraneous overrides ‘{}’, specify ‘override({})’",
                                    cur.name,
                                    extra.join(","),
                                    source_override
                                ),
                            ));
                        }
                    }

                    // FIXME: check override visibility/mutability

                    override_needed.remove(&signature);
                } else if entry.len() == 1 {
                    // Solidity 0.5 does not require the override keyword at all, later versions so. Uniswap v2 does
                    // not specify override for implementing interfaces. As a compromise, only require override when
                    // not implementing an interface
                    if !ns.contracts[entry[0].0].is_interface() {
                        ns.diagnostics.push(ast::Diagnostic::error(
                            cur.loc,
                            format!("function ‘{}’ should specify ‘override’", cur.name),
                        ));
                    }

                    // FIXME: check override visibility/mutability

                    override_needed.remove(&signature);
                } else {
                    ns.diagnostics.push(ast::Diagnostic::error(
                        cur.loc,
                        format!(
                            "function ‘{}’ should specify override list ‘override({})’",
                            cur.name, source_override
                        ),
                    ));
                }
            } else {
                let previous_defs = ns.contracts[contract_no]
                    .all_functions
                    .keys()
                    .filter(|function_no| {
                        let func = &ns.functions[**function_no];

                        func.ty != pt::FunctionTy::Constructor && func.signature == signature
                    })
                    .cloned()
                    .collect::<Vec<usize>>();

                if previous_defs.is_empty() && cur.is_override.is_some() {
                    ns.diagnostics.push(ast::Diagnostic::error(
                        cur.loc,
                        format!("function ‘{}’ does not override anything", cur.name),
                    ));
                    continue;
                }

                // a function without body needs an override
                if previous_defs.is_empty() && !cur.has_body {
                    override_needed
                        .insert(signature.clone(), vec![(base_contract_no, function_no)]);
                    continue;
                }

                for prev in previous_defs.into_iter() {
                    let func_prev = &ns.functions[prev];

                    if Some(base_contract_no) == func_prev.contract_no {
                        ns.diagnostics.push(ast::Diagnostic::error_with_note(
                            cur.loc,
                            format!(
                                "function ‘{}’ overrides function in same contract",
                                cur.name
                            ),
                            func_prev.loc,
                            format!("previous definition of ‘{}’", func_prev.name),
                        ));

                        continue;
                    }

                    if func_prev.ty != cur.ty {
                        ns.diagnostics.push(ast::Diagnostic::error_with_note(
                            cur.loc,
                            format!("{} ‘{}’ overrides {}", cur.ty, cur.name, func_prev.ty,),
                            func_prev.loc,
                            format!("previous definition of ‘{}’", func_prev.name),
                        ));

                        continue;
                    }

                    if func_prev
                        .params
                        .iter()
                        .zip(cur.params.iter())
                        .any(|(a, b)| a.ty != b.ty)
                    {
                        ns.diagnostics.push(ast::Diagnostic::error_with_note(
                            cur.loc,
                            format!(
                                "{} ‘{}’ overrides {} with different argument types",
                                cur.ty, cur.name, func_prev.ty,
                            ),
                            func_prev.loc,
                            format!("previous definition of ‘{}’", func_prev.name),
                        ));

                        continue;
                    }

                    if func_prev
                        .returns
                        .iter()
                        .zip(cur.returns.iter())
                        .any(|(a, b)| a.ty != b.ty)
                    {
                        ns.diagnostics.push(ast::Diagnostic::error_with_note(
                            cur.loc,
                            format!(
                                "{} ‘{}’ overrides {} with different return types",
                                cur.ty, cur.name, func_prev.ty,
                            ),
                            func_prev.loc,
                            format!("previous definition of ‘{}’", func_prev.name),
                        ));

                        continue;
                    }

                    // if a function needs an override, it was defined in a contract, not outside
                    let prev_contract_no = func_prev.contract_no.unwrap();

                    if let Some((loc, override_list)) = &cur.is_override {
                        if !func_prev.is_virtual {
                            ns.diagnostics.push(ast::Diagnostic::error_with_note(
                                cur.loc,
                                format!(
                                    "function ‘{}’ overrides function which is not virtual",
                                    cur.name
                                ),
                                func_prev.loc,
                                format!("previous definition of function ‘{}’", func_prev.name),
                            ));

                            continue;
                        }

                        if !override_list.is_empty() && !override_list.contains(&prev_contract_no) {
                            ns.diagnostics.push(ast::Diagnostic::error_with_note(
                                *loc,
                                format!(
                                    "function ‘{}’ override list does not contain ‘{}’",
                                    cur.name, ns.contracts[prev_contract_no].name
                                ),
                                func_prev.loc,
                                format!("previous definition of function ‘{}’", func_prev.name),
                            ));
                            continue;
                        }
                    } else if cur.has_body {
                        if let Some(entry) = override_needed.get_mut(&signature) {
                            entry.push((base_contract_no, function_no));
                        } else {
                            override_needed.insert(
                                signature.clone(),
                                vec![(prev_contract_no, prev), (base_contract_no, function_no)],
                            );
                        }
                        continue;
                    }
                }
            }

            if cur.is_override.is_some() || cur.is_virtual {
                ns.contracts[contract_no]
                    .virtual_functions
                    .insert(signature, function_no);
            }

            ns.contracts[contract_no]
                .all_functions
                .insert(function_no, usize::MAX);
        }
    }

    ns.contracts[contract_no].fixed_layout_size = slot;

    for list in override_needed.values() {
        let func = &ns.functions[list[0].1];

        // interface or abstract contracts are allowed to have virtual function which are not overriden
        if func.is_virtual && !ns.contracts[contract_no].is_concrete() {
            continue;
        }

        // virtual functions without a body
        if list.len() == 1 {
            let loc = ns.contracts[contract_no].loc;
            match func.ty {
                pt::FunctionTy::Fallback | pt::FunctionTy::Receive => {
                    ns.diagnostics.push(ast::Diagnostic::error_with_note(
                        loc,
                        format!(
                            "contract ‘{}’ missing override for ‘{}’ function",
                            ns.contracts[contract_no].name, func.ty
                        ),
                        func.loc,
                        format!("declaration of ‘{}’ function", func.ty),
                    ));
                }
                _ => ns.diagnostics.push(ast::Diagnostic::error_with_note(
                    loc,
                    format!(
                        "contract ‘{}’ missing override for function ‘{}’",
                        ns.contracts[contract_no].name, func.name
                    ),
                    func.loc,
                    format!("declaration of function ‘{}’", func.name),
                )),
            }

            continue;
        }

        let notes = list
            .iter()
            .skip(1)
            .map(|(_, function_no)| {
                let func = &ns.functions[*function_no];

                ast::Note {
                    pos: func.loc,
                    message: format!("previous definition of function ‘{}’", func.name),
                }
            })
            .collect();

        ns.diagnostics.push(ast::Diagnostic::error_with_notes(
            func.loc,
            format!(
                "function ‘{}’ with this signature already defined",
                func.name
            ),
            notes,
        ));
    }
}

/// Resolve functions declarations, constructor declarations, and contract variables
/// This returns a list of function bodies to resolve
fn resolve_declarations<'a>(
    def: &'a pt::ContractDefinition,
    file_no: usize,
    contract_no: usize,
    ns: &mut ast::Namespace,
) -> Vec<(usize, usize, &'a pt::FunctionDefinition)> {
    ns.diagnostics.push(ast::Diagnostic::debug(
        def.loc,
        format!("found {} ‘{}’", def.ty, def.name.name),
    ));

    let mut function_no_bodies = Vec::new();
    let mut resolve_bodies = Vec::new();

    // resolve function signatures
    for parts in &def.parts {
        if let pt::ContractPart::FunctionDefinition(ref f) = parts {
            if let Some(function_no) =
                functions::contract_function(def, f, file_no, contract_no, ns)
            {
                if f.body.is_some() {
                    resolve_bodies.push((contract_no, function_no, f.as_ref()));
                } else {
                    function_no_bodies.push(function_no);
                }
            }
        }
    }

    if let pt::ContractTy::Contract(loc) = &def.ty {
        if !function_no_bodies.is_empty() {
            let notes = function_no_bodies
                .into_iter()
                .map(|function_no| ast::Note {
                    pos: ns.functions[function_no].loc,
                    message: format!(
                        "location of function ‘{}’ with no body",
                        ns.functions[function_no].name
                    ),
                })
                .collect::<Vec<ast::Note>>();

            ns.diagnostics.push(ast::Diagnostic::error_with_notes(
                    *loc,
                    format!(
                        "contract should be marked ‘abstract contract’ since it has {} functions with no body",
                        notes.len()
                    ),
                    notes,
                ));
        }
    }

    // resolve state variables
    variables::contract_variables(&def, file_no, contract_no, ns);

    resolve_bodies
}

/// Resolve the using declarations in a contract
fn resolve_using(
    contracts: &[(usize, &pt::ContractDefinition)],
    file_no: usize,
    ns: &mut ast::Namespace,
) {
    for (contract_no, def) in contracts {
        for part in &def.parts {
            if let pt::ContractPart::Using(using) = part {
                if let Some(library_no) = ns.resolve_contract(file_no, &using.library) {
                    if !ns.contracts[library_no].is_library() {
                        ns.diagnostics.push(ast::Diagnostic::error(
                            using.library.loc,
                            format!(
                                "library expected but {} ‘{}’ found",
                                ns.contracts[library_no].ty, using.library.name
                            ),
                        ));

                        continue;
                    }

                    let ty = if let Some(expr) = &using.ty {
                        let mut diagnostics = Vec::new();

                        match ns.resolve_type(
                            file_no,
                            Some(*contract_no),
                            false,
                            expr,
                            &mut diagnostics,
                        ) {
                            Ok(ast::Type::Contract(contract_no)) => {
                                ns.diagnostics.push(ast::Diagnostic::error(
                                    using.library.loc,
                                    format!(
                                        "using library ‘{}’ to extend {} type not possible",
                                        using.library.name, ns.contracts[contract_no].ty
                                    ),
                                ));
                                continue;
                            }
                            Ok(ty) => Some(ty),
                            Err(_) => {
                                ns.diagnostics.extend(diagnostics);
                                continue;
                            }
                        }
                    } else {
                        None
                    };

                    ns.contracts[*contract_no].using.push((library_no, ty));
                } else {
                    ns.diagnostics.push(ast::Diagnostic::error(
                        using.library.loc,
                        format!("library ‘{}’ not found", using.library.name),
                    ));
                }
            }
        }
    }
}

/// Resolve contract functions bodies
fn resolve_bodies(
    bodies: Vec<(usize, usize, &pt::FunctionDefinition)>,
    file_no: usize,
    ns: &mut ast::Namespace,
) -> bool {
    let mut broken = false;

    for (contract_no, function_no, def) in bodies {
        if statements::resolve_function_body(def, file_no, Some(contract_no), function_no, ns)
            .is_err()
        {
            broken = true;
        }
    }

    broken
}

#[derive(Debug)]
pub struct BaseOrModifier<'a> {
    pub loc: &'a pt::Loc,
    pub defined_constructor_no: Option<usize>,
    pub calling_constructor_no: usize,
    pub args: &'a Vec<ast::Expression>,
}

// walk the list of base contracts and collect all the base constructor arguments
pub fn collect_base_args<'a>(
    contract_no: usize,
    constructor_no: Option<usize>,
    base_args: &mut HashMap<usize, BaseOrModifier<'a>>,
    diagnostics: &mut HashSet<ast::Diagnostic>,
    ns: &'a ast::Namespace,
) {
    let contract = &ns.contracts[contract_no];

    if let Some(defined_constructor_no) = constructor_no {
        let constructor = &ns.functions[defined_constructor_no];

        for (base_no, (loc, constructor_no, args)) in &constructor.bases {
            if let Some(prev_args) = base_args.get(base_no) {
                diagnostics.insert(ast::Diagnostic::error_with_note(
                    *loc,
                    format!(
                        "duplicate argument for base contract ‘{}’",
                        ns.contracts[*base_no].name
                    ),
                    *prev_args.loc,
                    format!(
                        "previous argument for base contract ‘{}’",
                        ns.contracts[*base_no].name
                    ),
                ));
            } else {
                base_args.insert(
                    *base_no,
                    BaseOrModifier {
                        loc,
                        defined_constructor_no: Some(defined_constructor_no),
                        calling_constructor_no: *constructor_no,
                        args,
                    },
                );

                collect_base_args(*base_no, Some(*constructor_no), base_args, diagnostics, ns);
            }
        }
    }

    for base in &contract.bases {
        if let Some((constructor_no, args)) = &base.constructor {
            if let Some(prev_args) = base_args.get(&base.contract_no) {
                diagnostics.insert(ast::Diagnostic::error_with_note(
                    base.loc,
                    format!(
                        "duplicate argument for base contract ‘{}’",
                        ns.contracts[base.contract_no].name
                    ),
                    *prev_args.loc,
                    format!(
                        "previous argument for base contract ‘{}’",
                        ns.contracts[base.contract_no].name
                    ),
                ));
            } else {
                base_args.insert(
                    base.contract_no,
                    BaseOrModifier {
                        loc: &base.loc,
                        defined_constructor_no: None,
                        calling_constructor_no: *constructor_no,
                        args,
                    },
                );

                collect_base_args(
                    base.contract_no,
                    Some(*constructor_no),
                    base_args,
                    diagnostics,
                    ns,
                );
            }
        } else {
            collect_base_args(
                base.contract_no,
                ns.contracts[base.contract_no].no_args_constructor(ns),
                base_args,
                diagnostics,
                ns,
            );
        }
    }
}

/// Check if we have arguments for all the base contracts
fn check_base_args(contract_no: usize, ns: &mut ast::Namespace) {
    let contract = &ns.contracts[contract_no];

    if !contract.is_concrete() {
        return;
    }

    let mut diagnostics = HashSet::new();
    let base_args_needed = visit_bases(contract_no, ns)
        .into_iter()
        .filter(|base_no| {
            *base_no != contract_no && ns.contracts[*base_no].constructor_needs_arguments(ns)
        })
        .collect::<Vec<usize>>();

    if contract.have_constructor(ns) {
        for constructor_no in contract
            .functions
            .iter()
            .filter(|function_no| ns.functions[**function_no].is_constructor())
        {
            let mut base_args = HashMap::new();

            collect_base_args(
                contract_no,
                Some(*constructor_no),
                &mut base_args,
                &mut diagnostics,
                ns,
            );

            for base_no in &base_args_needed {
                if !base_args.contains_key(base_no) {
                    diagnostics.insert(ast::Diagnostic::error(
                        contract.loc,
                        format!(
                            "missing arguments to base contract ‘{}’ constructor",
                            ns.contracts[*base_no].name
                        ),
                    ));
                }
            }
        }
    } else {
        let mut base_args = HashMap::new();

        collect_base_args(contract_no, None, &mut base_args, &mut diagnostics, ns);

        for base_no in &base_args_needed {
            if !base_args.contains_key(base_no) {
                diagnostics.insert(ast::Diagnostic::error(
                    contract.loc,
                    format!(
                        "missing arguments to base contract ‘{}’ constructor",
                        ns.contracts[*base_no].name
                    ),
                ));
            }
        }
    }

    ns.diagnostics.extend(diagnostics.into_iter());
}
