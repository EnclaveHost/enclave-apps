#!/usr/bin/python3
"""Recognize typed OpenCL C kernels; unknown programs retain PoCL CPU execution.

This pass does not grant public-data approval. It proves only the supported
kernel's arithmetic/access pattern. Runtime geometry and alias checks plus the
measured service's data policy are separate mandatory checks before offload.
"""
import json
import os
from pathlib import Path
import resource
import subprocess
import sys
import struct
import tempfile

MAX_SOURCE = 1 << 20
MAX_AST = 32 << 20
MAX_IL = 4 << 20
ROOT = Path(__file__).resolve().parent


def limits():
    # Native applications already run under service limits (notably an 8 MiB
    # file-size hard limit). Raising that inherited hard limit in preexec_fn
    # fails before Clang starts and silently disables all source recognition.
    # Respect both inherited soft and hard limits; only tighten them here.
    for kind, desired in ((resource.RLIMIT_CPU, 10), (resource.RLIMIT_AS, 2 << 30),
                          (resource.RLIMIT_FSIZE, MAX_AST), (resource.RLIMIT_CORE, 0)):
        inherited = resource.getrlimit(kind)
        bound = min([desired] + [value for value in inherited if value != resource.RLIM_INFINITY])
        resource.setrlimit(kind, (bound, bound))


def ast(source, clang='/usr/bin/clang-18', flags=('-cl-std=CL1.2',)):
    if len(source.encode()) > MAX_SOURCE or '\0' in source:
        raise ValueError('source limit')
    with tempfile.TemporaryDirectory(prefix='shield-kernel-') as directory:
        root = Path(directory)
        (root / 'kernel.cl').write_text(source)
        with (root / 'ast.json').open('w+b') as output:
            result = subprocess.run([clang, '-x', 'cl', *flags,
                '-fsyntax-only', '-Xclang', '-ast-dump=json', str(root / 'kernel.cl')],
                stdout=output, stderr=subprocess.DEVNULL, timeout=15,
                preexec_fn=limits, env={'PATH': '/usr/bin:/bin', 'LANG': 'C'})
            if result.returncode:
                raise ValueError('kernel analysis failed')
            output.seek(0)
            return json.loads(output.read(MAX_AST))


def canonical_type(node):
    value = node.get('type', {})
    text = value.get('desugaredQualType', value.get('qualType', ''))
    return ' '.join(text.replace('__private', '').split())


def normalize(function):
    """Keep typed operations/casts, declaration identity, and all side effects.

    Only names, source locations, redundant parentheses, declaration grouping
    and single-statement braces are erased. In particular, narrowing casts,
    signedness, index changes, stores and extra calls cannot disappear.
    """
    symbols = {}
    def visit(node):
        kind = node.get('kind', 'Empty')
        if kind in ('OpenCLKernelAttr', 'DeviceKernelAttr'):
            return None
        if kind.endswith('Attr'):
            raise ValueError('kernel attributes need separate lowering')
        if kind == 'ParenExpr':
            return visit(node['inner'][0])
        if kind == 'DeclRefExpr':
            decl = node['referencedDecl']
            if decl['kind'] == 'FunctionDecl':
                if decl['name'] != 'get_global_id':
                    raise ValueError('unrecognized call')
                return [kind, decl['name'], canonical_type(node)]
            if decl['id'] not in symbols:
                raise ValueError('reference outside kernel')
            return [kind, symbols[decl['id']], canonical_type(node)]
        if kind in ('ParmVarDecl', 'VarDecl'):
            symbols[node['id']] = len(symbols)
        children = [x for n in node.get('inner', []) if (x := visit(n)) is not None]
        if kind == 'DeclStmt':
            return ['declarations', children]
        if kind == 'CompoundStmt':
            statements = []
            for child in children:
                statements.extend(child[1] if child[0] == 'declarations' else [child])
            if len(statements) == 1:
                return statements[0]
            return [kind, statements]
        if kind == 'FunctionDecl':
            return [kind, children]  # Function spelling is not its semantics.
        return [kind, canonical_type(node),
                {k: node[k] for k in ('opcode', 'value', 'castKind') if k in node}, children]
    return visit(function)


def kernels(tree):
    functions = [n for n in tree.get('inner', []) if n.get('kind') == 'FunctionDecl']
    # Builtins materialized by Clang's OpenCL builtin table do not appear as
    # source FunctionDecls here. A user redeclaration can attach an asm label
    # and redirect get_global_id to another function without changing the
    # call expression's spelling or type. Never lower that source.
    if any(f.get('name') == 'get_global_id' for f in functions):
        return []
    # A user definition of a builtin or a separately compiled helper must not
    # masquerade as the builtin semantics used by the approved pattern.
    for f in functions:
        if any(n.get('kind') == 'CompoundStmt' for n in f.get('inner', [])) and not any(
                n.get('kind') in ('OpenCLKernelAttr', 'DeviceKernelAttr') for n in f.get('inner', [])):
            return []
    return [f for f in functions if any(n.get('kind') in ('OpenCLKernelAttr', 'DeviceKernelAttr')
            for n in f.get('inner', []))]


def compile_source(source, options='', clang='/usr/bin/clang-18'):
    # Do not analyze different preprocessing/ABI flags from the program that
    # actually compiled. Additional build-option support needs matching tests.
    if options.strip() not in ('', '-cl-std=CL1.2'):
        return {}
    reference = normalize(kernels(ast((ROOT / 'gemm_i8.cl').read_text(), clang))[0])
    plans = {}
    for function in kernels(ast(source, clang)):
        try:
            matches = normalize(function) == reference
        except (ValueError, KeyError):
            matches = False
        if matches:
            plans[function['name']] = {
                'schema': 'shield-opencl-lowering/1', 'operation': 'gemm-i8-i8-i64',
                'arguments': {'a': 0, 'b': 1, 'c': 2, 'm': 3, 'n': 4, 'k': 5},
                'inputBound': 128, 'dimensions': 2,
                'requires': ['complete-zero-offset-grid', 'disjoint-output-storage',
                             'authenticated-public-rhs', 'static-no-wrap-field-bound'],
            }
    return plans


def compile_modules(modules):
    """Analyze all linked SPIR-V inputs; names/metadata alone grant no plan."""
    if not isinstance(modules, list) or not 1 <= len(modules) <= 8:
        return {}
    binaries = [bytes.fromhex(module) for module in modules]
    if sum(map(len, binaries)) > MAX_IL or any(not b or len(b) % 4 for b in binaries):
        return {}
    for binary in binaries:
        words = struct.unpack('<' + 'I' * (len(binary) // 4), binary)
        if len(words) < 5 or words[0] != 0x07230203:
            return {}
        offset = 5
        while offset < len(words):
            size, opcode = words[offset] >> 16, words[offset] & 0xffff
            if not size or size > len(words) - offset:
                return {}
            # Runtime specialization can change these values after IL creation.
            # Until specialization state is captured, no such module is lowered.
            if opcode in (48, 49, 50, 51, 52, 4462, 5104, 5148, 6092, 6251, 6252, 6253):
                return {}
            offset += size
    with tempfile.TemporaryDirectory(prefix='shield-ir-') as directory:
        root = Path(directory)
        bitcode = []
        def run(args, stdout=subprocess.DEVNULL):
            return subprocess.run(args, stdout=stdout, stderr=subprocess.DEVNULL,
                                  check=True, timeout=15, preexec_fn=limits,
                                  env={'PATH': '/usr/bin:/bin', 'LANG': 'C'})
        for index, data in enumerate(binaries):
            spirv = root / f'{index}.spv'
            bc = root / f'{index}.bc'
            spirv.write_bytes(data)
            run(['/usr/bin/spirv-val', str(spirv)])
            run(['/usr/bin/llvm-spirv-18', '-r', str(spirv), '-o', str(bc)])
            bitcode.append(str(bc))
        linked = root / 'linked.bc'
        run(['/usr/bin/llvm-link-18', *bitcode, '-o', str(linked)])
        result = run([str(ROOT / 'ir-compiler'), str(ROOT / 'gemm-reference.bc'),
                      str(linked)], stdout=subprocess.PIPE)
        if len(result.stdout) > 65536:
            return {}
        return json.loads(result.stdout)


def main():
    try:
        data = sys.stdin.buffer.read(2 * MAX_IL + 4096)
        request = json.loads(data)
        if set(request) == {'modules'}:
            result = compile_modules(request['modules'])
        elif set(request) == {'source', 'options'}:
            result = compile_source(request['source'], request['options'])
        else:
            raise ValueError('invalid compiler request')
    except (ValueError, KeyError, TypeError, OSError, subprocess.SubprocessError, IndexError):
        result = {}
    print(json.dumps(result, separators=(',', ':')))


if __name__ == '__main__':
    main()
