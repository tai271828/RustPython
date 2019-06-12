/*
 * Import mechanics
 */

use std::path::PathBuf;

use crate::compile;
use crate::frame::Scope;
use crate::obj::{objsequence, objstr};
use crate::pyobject::{ItemProtocol, PyResult};
use crate::util;
use crate::vm::VirtualMachine;

pub fn init_importlib(vm: &VirtualMachine) -> PyResult {
    let importlib = import_frozen(vm, "_frozen_importlib")?;
    let impmod = import_builtin(vm, "_imp")?;
    let install = vm.get_attribute(importlib.clone(), "_install")?;
    vm.invoke(install, vec![vm.sys_module.clone(), impmod])?;
    vm.import_func
        .replace(vm.get_attribute(importlib.clone(), "__import__")?);
    let install_external = vm.get_attribute(importlib.clone(), "_install_external_importers")?;
    vm.invoke(install_external, vec![])?;
    Ok(vm.get_none())
}

pub fn import_frozen(vm: &VirtualMachine, module_name: &str) -> PyResult {
    if let Some(frozen) = vm.frozen.borrow().get(module_name) {
        import_file(
            vm,
            module_name,
            format!("frozen {}", module_name),
            frozen.to_string(),
        )
    } else {
        Err(vm.new_import_error(format!("Cannot import frozen module {}", module_name)))
    }
}

pub fn import_builtin(vm: &VirtualMachine, module_name: &str) -> PyResult {
    let sys_modules = vm.get_attribute(vm.sys_module.clone(), "modules").unwrap();
    if let Some(make_module_func) = vm.stdlib_inits.borrow().get(module_name) {
        let module = make_module_func(vm);
        sys_modules.set_item(module_name, module.clone(), vm)?;
        Ok(module)
    } else {
        Err(vm.new_import_error(format!("Cannot import bultin module {}", module_name)))
    }
}

pub fn import_module(vm: &VirtualMachine, current_path: PathBuf, module_name: &str) -> PyResult {
    // Cached modules:
    let sys_modules = vm.get_attribute(vm.sys_module.clone(), "modules").unwrap();

    // First, see if we already loaded the module:
    if let Ok(module) = sys_modules.get_item(module_name.to_string(), vm) {
        Ok(module)
    } else if vm.frozen.borrow().contains_key(module_name) {
        import_frozen(vm, module_name)
    } else if vm.stdlib_inits.borrow().contains_key(module_name) {
        import_builtin(vm, module_name)
    } else {
        let notfound_error = vm.context().exceptions.module_not_found_error.clone();
        let import_error = vm.context().exceptions.import_error.clone();

        // Time to search for module in any place:
        let file_path = find_source(vm, current_path, module_name)
            .map_err(|e| vm.new_exception(notfound_error.clone(), e))?;
        let source = util::read_file(file_path.as_path())
            .map_err(|e| vm.new_exception(import_error.clone(), e.to_string()))?;

        import_file(
            vm,
            module_name,
            file_path.to_str().unwrap().to_string(),
            source,
        )
    }
}

pub fn import_file(
    vm: &VirtualMachine,
    module_name: &str,
    file_path: String,
    content: String,
) -> PyResult {
    let sys_modules = vm.get_attribute(vm.sys_module.clone(), "modules").unwrap();
    let code_obj = compile::compile(vm, &content, &compile::Mode::Exec, file_path.clone())
        .map_err(|err| vm.new_syntax_error(&err))?;
    // trace!("Code object: {:?}", code_obj);

    let attrs = vm.ctx.new_dict();
    attrs.set_item("__name__", vm.new_str(module_name.to_string()), vm)?;
    if !file_path.starts_with("frozen") {
        // TODO: Should be removed after precompiling frozen modules.
        attrs.set_item("__file__", vm.new_str(file_path), vm)?;
    }
    let module = vm.ctx.new_module(module_name, attrs.clone());

    // Store module in cache to prevent infinite loop with mutual importing libs:
    sys_modules.set_item(module_name, module.clone(), vm)?;

    // Execute main code in module:
    vm.run_code_obj(code_obj, Scope::with_builtins(None, attrs, vm))?;
    Ok(module)
}

fn find_source(vm: &VirtualMachine, current_path: PathBuf, name: &str) -> Result<PathBuf, String> {
    let sys_path = vm.get_attribute(vm.sys_module.clone(), "path").unwrap();
    let mut paths: Vec<PathBuf> = objsequence::get_elements_list(&sys_path)
        .iter()
        .map(|item| PathBuf::from(objstr::get_value(item)))
        .collect();

    paths.insert(0, current_path);

    let rel_name = name.replace('.', "/");
    let suffixes = [".py", "/__init__.py"];
    let mut file_paths = vec![];
    for path in paths {
        for suffix in suffixes.iter() {
            let mut file_path = path.clone();
            file_path.push(format!("{}{}", rel_name, suffix));
            file_paths.push(file_path);
        }
    }

    match file_paths.iter().find(|p| p.exists()) {
        Some(path) => Ok(path.to_path_buf()),
        None => Err(format!("No module named '{}'", name)),
    }
}
