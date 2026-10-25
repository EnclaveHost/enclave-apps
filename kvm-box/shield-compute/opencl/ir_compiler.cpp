// Structural lowering proof for the initial CUDA integer GEMM family.
// Both inputs are LLVM bitcode from the pinned SPIR-V translator. This tool
// never authorizes public weights or bypasses runtime geometry/alias checks.
#include <llvm/IR/Constants.h>
#include <llvm/IR/DebugInfo.h>
#include <llvm/IR/InstIterator.h>
#include <llvm/IR/Instructions.h>
#include <llvm/IR/LLVMContext.h>
#include <llvm/IR/Module.h>
#include <llvm/IR/Verifier.h>
#include <llvm/IRReader/IRReader.h>
#include <llvm/Passes/PassBuilder.h>
#include <llvm/Support/JSON.h>
#include <llvm/Support/SourceMgr.h>
#include <llvm/Support/raw_ostream.h>
#include <llvm/Transforms/Utils/Cloning.h>
#include <llvm/Transforms/Utils/FunctionComparator.h>
#include <set>
#include <functional>

using namespace llvm;

static bool permittedGlobals(Module &module) {
  // chipStar emits an unused constant capability marker before its runtime
  // preprocessing step. An unreferenced integer constant has no execution or
  // initialization effects. No mutable or referenced global is discarded.
  for (auto it = module.global_begin(); it != module.global_end();) {
    GlobalVariable &global = *it++;
    if (global.isConstant() && global.use_empty() && global.hasInitializer() &&
        isa<ConstantInt>(global.getInitializer()) && !global.isExternallyInitialized())
      global.eraseFromParent();
  }
  return module.global_empty() && module.alias_empty() && module.ifunc_empty() &&
         module.getModuleInlineAsm().empty();
}

static bool permittedModule(Module &module) {
  if (module.getTargetTriple() != "spir64-unknown-unknown" ||
      !permittedGlobals(module) || verifyModule(module, &errs()))
    return false;
  const std::set<std::string> builtins = {
      "_Z12get_group_idj", "_Z14get_local_sizej", "_Z12get_local_idj"};
  size_t instructions = 0;
  for (Function &f : module) {
    if (f.isDeclaration()) {
      if (!builtins.count(f.getName().str()) ||
          f.getCallingConv() != CallingConv::SPIR_FUNC || f.isVarArg() ||
          !f.getReturnType()->isIntegerTy(64) || f.arg_size() != 1 ||
          !f.getArg(0)->getType()->isIntegerTy(32))
        return false;
    } else {
      // Never confuse an imported indexing builtin with a user definition.
      if (builtins.count(f.getName().str()))
        return false;
      if (f.getCallingConv() != CallingConv::SPIR_KERNEL && !f.hasLocalLinkage())
        return false;
      SmallVector<std::pair<unsigned, MDNode *>, 8> metadata;
      f.getAllMetadata(metadata);
      SmallVector<StringRef, 32> names;
      module.getContext().getMDKindNames(names);
      for (auto [kind, node] : metadata) {
        (void)node;
        auto name = names[kind];
        if (name != "dbg" && name != "kernel_arg_addr_space" &&
            name != "kernel_arg_access_qual" && name != "kernel_arg_type" &&
            name != "kernel_arg_base_type" && name != "kernel_arg_type_qual" &&
            name != "kernel_arg_name" && name != "spirv.ParameterDecorations")
          return false;
      }
      for (Instruction &i : llvm::instructions(f)) {
        if (++instructions > 20000)
          return false;
        if (auto *call = dyn_cast<CallBase>(&i))
          if (!call->getCalledFunction() || call->isInlineAsm())
            return false;
      }
    }
  }
  return true;
}

static std::unique_ptr<Module> kernelClosure(Module &module, StringRef name) {
  auto result = CloneModule(module);
  if (!permittedGlobals(*result))
    return nullptr;
  Function *entry = result->getFunction(name);
  std::set<Function *> reachable;
  SmallVector<Function *, 16> pending{entry};
  while (!pending.empty()) {
    Function *function = pending.pop_back_val();
    if (!reachable.insert(function).second)
      continue;
    for (Instruction &i : instructions(function)) {
      if (auto *call = dyn_cast<CallBase>(&i)) {
        if (!call->getCalledFunction() || call->isInlineAsm())
          return nullptr;
        pending.push_back(call->getCalledFunction());
      }
    }
  }
  SmallVector<Function *, 16> remove;
  for (Function &f : *result)
    if (!reachable.count(&f)) {
      remove.push_back(&f);
      f.dropAllReferences();
    }
  for (Function *f : remove) {
    // A function address used outside direct calls is not silently erased.
    if (!f->use_empty())
      return nullptr;
    f->eraseFromParent();
  }
  return result;
}

static bool normalize(Module &module) {
  StripDebugInfo(module);
  // Only optimization-control attributes are removed. Numeric, memory and
  // poison-related attributes remain part of the function comparison.
  for (Function &f : module) {
    f.removeFnAttr(Attribute::OptimizeNone);
    f.removeFnAttr(Attribute::NoInline);
  }
  unsigned expansions = 0;
  for (Function &kernel : module) {
    if (kernel.getCallingConv() != CallingConv::SPIR_KERNEL)
      continue;
    while (true) {
      CallBase *next = nullptr;
      for (Instruction &i : instructions(kernel)) {
        auto *call = dyn_cast<CallBase>(&i);
        if (call && !call->getCalledFunction()->isDeclaration()) {
          next = call;
          break;
        }
      }
      if (!next)
        break;
      if (++expansions > 64 || !next->getCalledFunction()->hasLocalLinkage())
        return false;
      InlineFunctionInfo info;
      if (!InlineFunction(*next, info).isSuccess())
        return false;
      if (kernel.getInstructionCount() > 20000)
        return false;
    }
  }
  LoopAnalysisManager loops;
  FunctionAnalysisManager functions;
  CGSCCAnalysisManager sccs;
  ModuleAnalysisManager modules;
  PassBuilder builder;
  builder.registerModuleAnalyses(modules);
  builder.registerCGSCCAnalyses(sccs);
  builder.registerFunctionAnalyses(functions);
  builder.registerLoopAnalyses(loops);
  builder.crossRegisterProxies(loops, functions, sccs, modules);
  ModulePassManager passes;
  if (auto error = builder.parsePassPipeline(
          passes, "function(mem2reg,instcombine,simplifycfg,dce)")) {
    consumeError(std::move(error));
    return false;
  }
  passes.run(module, modules);
  // Inlining can place independent address/index calculations in a different
  // order. Schedule only side-effect-free integer/address expressions at their
  // first use, preserving all calls, loads, stores and other observable order.
  auto pure = [](Instruction *i) {
    return isa<GetElementPtrInst>(i) || isa<CastInst>(i) || isa<ICmpInst>(i) ||
           isa<SelectInst>(i) ||
           (isa<BinaryOperator>(i) && i->getType()->isIntegerTy() &&
            i->getOpcode() != Instruction::SDiv && i->getOpcode() != Instruction::UDiv &&
            i->getOpcode() != Instruction::SRem && i->getOpcode() != Instruction::URem);
  };
  for (Function &f : module)
    for (BasicBlock &block : f) {
      SmallVector<Instruction *, 32> anchors;
      for (Instruction &i : block)
        if (!pure(&i))
          anchors.push_back(&i);
      std::set<Instruction *> scheduled;
      for (Instruction *anchor : anchors) {
        std::function<void(Value *)> visit = [&](Value *value) {
          auto *i = dyn_cast<Instruction>(value);
          if (!i || i->getParent() != &block || !pure(i) || scheduled.count(i))
            return;
          for (Value *operand : i->operands())
            visit(operand);
          i->moveBefore(anchor);
          scheduled.insert(i);
        };
        // Phi operands refer to predecessor edges, not an insertion point at
        // the start of this block.
        if (!isa<PHINode>(anchor))
          for (Value *operand : anchor->operands())
            visit(operand);
      }
    }
  return !verifyModule(module, &errs());
}

int main(int argc, char **argv) {
  json::Object plans;
  if (argc != 3 && argc != 4) {
    outs() << "{}\n";
    return 0;
  }
  LLVMContext context;
  SMDiagnostic diagnostics;
  auto reference = parseIRFile(argv[1], diagnostics, context);
  auto candidate = parseIRFile(argv[2], diagnostics, context);
  if (!reference || !candidate || !permittedModule(*reference) ||
      candidate->getTargetTriple() != reference->getTargetTriple() ||
      verifyModule(*candidate, &errs()) ||
      reference->getDataLayoutStr() != candidate->getDataLayoutStr() ||
      !normalize(*reference)) {
    outs() << "{}\n";
    return 0;
  }
  Function *pattern = reference->getFunction("gemm");
  if (!pattern || pattern->isDeclaration() ||
      pattern->getCallingConv() != CallingConv::SPIR_KERNEL) {
    outs() << "{}\n";
    return 0;
  }
  for (Function &entry : *candidate) {
    if (entry.isDeclaration() || entry.getCallingConv() != CallingConv::SPIR_KERNEL)
      continue;
    auto isolated = kernelClosure(*candidate, entry.getName());
    if (!isolated || !permittedModule(*isolated) || !normalize(*isolated))
      continue;
    // FunctionComparator uses identity for external globals. Unify declarations
    // only after checking the exact imported builtin name, ABI and attributes.
    bool imports_match = true;
    for (Function &f : *isolated) {
      if (!f.isDeclaration())
        continue;
      Function *known = reference->getFunction(f.getName());
      if (!known || !known->isDeclaration() ||
          f.getFunctionType() != known->getFunctionType() ||
          f.getAttributes() != known->getAttributes() ||
          f.getCallingConv() != known->getCallingConv()) {
        imports_match = false;
        break;
      }
      f.replaceAllUsesWith(known);
    }
    Function *f = isolated->getFunction(entry.getName());
    if (!imports_match || f->getFunctionType() != pattern->getFunctionType())
      continue;
    if (argc == 4 && StringRef(argv[3]) == "--dump")
      errs() << "REFERENCE\n" << *pattern << "\nCANDIDATE\n" << *f;
    GlobalNumberState globals;
    FunctionComparator comparison(pattern, f, &globals);
    if (comparison.compare() == 0)
      plans[f->getName().str()] = json::Object{
          {"schema", "shield-opencl-lowering/1"},
          {"operation", "gemm-i8-i8-i64"},
          {"arguments", json::Object{{"a", 0}, {"b", 1}, {"c", 2},
                                      {"m", 3}, {"n", 4}, {"k", 5}}},
          {"requires", json::Array{"complete-zero-offset-grid", "32-bit-grid",
                                   "disjoint-output-storage", "authenticated-public-rhs",
                                   "static-no-wrap-field-bound", "verified-linkage"}}};
  }
  outs() << formatv("{0}\n", json::Value(std::move(plans)));
}
