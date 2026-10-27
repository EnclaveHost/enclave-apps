/* Experimental proof of bounded integer affine prepasses, after NIR SSA
 * normalization. A successful proof is not policy approval or driver admission.
 * All arithmetic is in Z/(2^32); the final bounded result is separately known.
 * No runtime input values, shader names or debug information are consulted. */
#include "shield_prepass_nir.h"
#include <string.h>
#include <vulkan/vulkan_core.h>

enum form { INVALID, AFFINE, RAW };
/* Constant, local invocation x, four centered inputs, and workgroup x.
 * Keeping local/group independent prevents treating local ID as global ID. */
struct value { enum form form; uint32_t coefficient[7]; unsigned raw; };
static struct value bad(void) { return (struct value){0}; }
static struct value constant(uint32_t n) {
   struct value v = {.form=AFFINE}; v.coefficient[0]=n; return v;
}
static bool scalar(struct value v) {
   if (v.form != AFFINE) return false;
   for (unsigned i=1;i<7;i++) if (v.coefficient[i]) return false;
   return true;
}
static bool equals(struct value a, struct value b) {
   return a.form==AFFINE && b.form==AFFINE && !memcmp(a.coefficient,b.coefficient,sizeof a.coefficient);
}
static struct value evaluate(nir_def *def, unsigned component, unsigned depth, unsigned *budget, enum shield_nir_contract contract, unsigned groups);
static bool immediate(nir_def *def, uint32_t n) {
   return def && def->bit_size==32 && def->num_components==1 &&
      def->parent_instr->type==nir_instr_type_load_const &&
      nir_instr_as_load_const(def->parent_instr)->value[0].u32==n;
}
/* Exact logical storage layout and descriptor. No physical/BDA pointers. */
static bool storage(nir_deref_instr *d, unsigned binding, struct value *index, unsigned depth, unsigned *budget, enum shield_nir_contract contract, unsigned groups) {
   if (!d || depth>64 || d->deref_type!=nir_deref_type_array || d->modes!=nir_var_mem_ssbo) return false;
   nir_deref_instr *field=nir_deref_instr_parent(d);
   if (!field || field->deref_type!=nir_deref_type_struct || field->strct.index!=0) return false;
   nir_deref_instr *base=nir_deref_instr_parent(field);
   if (!base || base->deref_type!=nir_deref_type_cast || base->modes!=nir_var_mem_ssbo ||
       !glsl_type_is_struct_or_ifc(base->type) || glsl_get_length(base->type)!=1 ||
       glsl_get_struct_field_offset(base->type,0)!=0 || !glsl_type_is_unsized_array(field->type) ||
       glsl_get_explicit_stride(field->type)!=((binding && contract==SHIELD_NIR_FIXED64_V1)?16:4) ||
       !glsl_type_is_integer_32(d->type) || glsl_get_vector_elements(d->type)!=((binding && contract==SHIELD_NIR_FIXED64_V1)?4:1)) return false;
   nir_def *address=base->parent.ssa;
   if (!address || address->parent_instr->type!=nir_instr_type_intrinsic) return false;
   nir_intrinsic_instr *desc=nir_instr_as_intrinsic(address->parent_instr);
   if (desc->intrinsic!=nir_intrinsic_load_vulkan_descriptor ||
       nir_intrinsic_desc_type(desc)!=VK_DESCRIPTOR_TYPE_STORAGE_BUFFER) return false;
   nir_def *resource=desc->src[0].ssa;
   if (!resource || resource->parent_instr->type!=nir_instr_type_intrinsic) return false;
   nir_intrinsic_instr *ref=nir_instr_as_intrinsic(resource->parent_instr);
   if (ref->intrinsic!=nir_intrinsic_vulkan_resource_index || nir_intrinsic_desc_set(ref)!=(contract==SHIELD_NIR_ZINK64_V1?3:0) ||
       nir_intrinsic_binding(ref)!=(contract==SHIELD_NIR_ZINK64_V1?0:binding) || nir_intrinsic_desc_type(ref)!=VK_DESCRIPTOR_TYPE_STORAGE_BUFFER ||
       !immediate(ref->src[0].ssa,contract==SHIELD_NIR_ZINK64_V1?binding:0)) return false;
   *index=evaluate(d->arr.index.ssa,0,depth+1,budget,contract,groups); return index->form==AFFINE;
}
static struct value alu_source(nir_alu_instr *alu,unsigned source,unsigned component,unsigned depth, unsigned *budget, enum shield_nir_contract contract, unsigned groups) {
   return evaluate(alu->src[source].src.ssa,alu->src[source].swizzle[component],depth+1,budget,contract,groups);
}
static struct value evaluate(nir_def *def, unsigned component, unsigned depth, unsigned *budget, enum shield_nir_contract contract, unsigned groups) {
   if (!*budget) return bad();
   --*budget;
   if (!def || depth>64 || component>=def->num_components || def->bit_size!=32) return bad();
   nir_instr *instr=def->parent_instr;
   if (instr->type==nir_instr_type_load_const) return constant(nir_instr_as_load_const(instr)->value[component].u32);
   if (instr->type==nir_instr_type_intrinsic) {
      nir_intrinsic_instr *load=nir_instr_as_intrinsic(instr);
      if (load->intrinsic==nir_intrinsic_load_global_invocation_id ||
          load->intrinsic==nir_intrinsic_load_local_invocation_id ||
          load->intrinsic==nir_intrinsic_load_workgroup_id) {
         struct value v=constant(0);
         if (!component) {
            if (load->intrinsic!=nir_intrinsic_load_workgroup_id) v.coefficient[1]=1;
            if (groups>1 && load->intrinsic!=nir_intrinsic_load_local_invocation_id)
               v.coefficient[6]=load->intrinsic==nir_intrinsic_load_global_invocation_id?64:1;
         }
         return v;
      }
      if (load->intrinsic!=nir_intrinsic_load_deref || nir_intrinsic_access(load)&(ACCESS_VOLATILE|ACCESS_COHERENT)) return bad();
      nir_deref_instr *d=nir_src_as_deref(load->src[0]);
      if (!d) return bad();
      if (d->deref_type==nir_deref_type_var && d->modes==nir_var_system_value &&
          (d->var->data.location==SYSTEM_VALUE_GLOBAL_INVOCATION_ID ||
           d->var->data.location==SYSTEM_VALUE_LOCAL_INVOCATION_ID ||
           d->var->data.location==SYSTEM_VALUE_WORKGROUP_ID)) {
         struct value v=constant(0);
         if (!component) {
            if (d->var->data.location!=SYSTEM_VALUE_WORKGROUP_ID) v.coefficient[1]=1;
            if (groups>1 && d->var->data.location!=SYSTEM_VALUE_LOCAL_INVOCATION_ID)
               v.coefficient[6]=d->var->data.location==SYSTEM_VALUE_GLOBAL_INVOCATION_ID?64:1;
         }
         return v;
      }
      struct value index;
      if (component || !storage(d,0,&index,depth+1,budget,contract,groups) || index.coefficient[1]!=4 || index.coefficient[6]!=(groups>1?256:0) || index.coefficient[0]>3) return bad();
      for (unsigned i=2;i<6;i++) if(index.coefficient[i]) return bad();
      return (struct value){.form=RAW,.raw=index.coefficient[0]};
   }
   if (instr->type!=nir_instr_type_alu) return bad();
   nir_alu_instr *alu=nir_instr_as_alu(instr);
   if (alu->op==nir_op_vec2 || alu->op==nir_op_vec3 || alu->op==nir_op_vec4)
      return alu_source(alu,component,0,depth,budget,contract,groups);
   struct value a=alu_source(alu,0,component,depth,budget,contract,groups),b=bad();
   if (alu->op==nir_op_mov) return a;
   if (alu->op==nir_op_ineg && a.form==AFFINE) {
      for (unsigned i=0;i<7;i++) a.coefficient[i]=0u-a.coefficient[i];
      return a;
   }
   if (nir_op_infos[alu->op].num_inputs!=2) return bad();
   b=alu_source(alu,1,component,depth,budget,contract,groups);
   if ((alu->op==nir_op_iand && a.form==RAW && scalar(b) && b.coefficient[0]==255) ||
       (alu->op==nir_op_extract_u8 && a.form==RAW && scalar(b) && b.coefficient[0]==0)) {
      struct value v=constant(128); v.coefficient[2+a.raw]=1; return v;
   }
   if (alu->op==nir_op_iand && b.form==RAW && scalar(a) && a.coefficient[0]==255) {
      struct value v=constant(128); v.coefficient[2+b.raw]=1; return v;
   }
   if (a.form!=AFFINE || b.form!=AFFINE) return bad();
   /* Normalization can turn (row<<26)>>26 into row&63. A low-bit
    * mask is an identity only when the complete public address interval
    * fits. Never use runtime/private values to justify that identity. */
   if (alu->op==nir_op_iand && (scalar(a)||scalar(b))) {
      if (!scalar(b)) { struct value temp=a;a=b;b=temp; }
      for(unsigned i=2;i<6;i++) if(a.coefficient[i]) return bad();
      uint32_t mask=b.coefficient[0];
      uint64_t maximum=(uint64_t)a.coefficient[0]+63ull*a.coefficient[1]+(uint64_t)(groups-1)*a.coefficient[6];
      if ((mask & (mask+1u)) || maximum>mask) return bad();
      return a;
   }
   if (alu->op==nir_op_iadd || alu->op==nir_op_isub) {
      for (unsigned i=0;i<7;i++) a.coefficient[i]+=alu->op==nir_op_iadd?b.coefficient[i]:0u-b.coefficient[i];
      return a;
   }
   if (alu->op==nir_op_ishl && scalar(b) && b.coefficient[0]<32) {
      for(unsigned i=0;i<7;i++) a.coefficient[i]<<=b.coefficient[0];
      return a;
   }
   /* Only proven nonnegative public row addresses can be divided this way.
    * Never distribute a shift over private values or modular overflow. */
   if (alu->op==nir_op_ushr && scalar(b) && b.coefficient[0]<32) {
      for(unsigned i=2;i<6;i++) if(a.coefficient[i]) return bad();
      uint64_t maximum=(uint64_t)a.coefficient[0]+63ull*a.coefficient[1]+(uint64_t)(groups-1)*a.coefficient[6];
      uint32_t mask=(1u<<b.coefficient[0])-1u;
      if(maximum>UINT32_MAX || (a.coefficient[0]&mask) || (a.coefficient[1]&mask) || (a.coefficient[6]&mask)) return bad();
      a.coefficient[0]>>=b.coefficient[0]; a.coefficient[1]>>=b.coefficient[0]; a.coefficient[6]>>=b.coefficient[0]; return a;
   }
   if (alu->op==nir_op_imul && (scalar(a)||scalar(b))) {
      if (!scalar(b)) { struct value temp=a;a=b;b=temp; }
      for(unsigned i=0;i<7;i++) a.coefficient[i]*=b.coefficient[0];
      return a;
   }
   return bad();
}
static enum shield_nir_reason prove(nir_shader *nir, enum shield_nir_contract contract, struct shield_affine64 *transform, unsigned groups) {
   if (!nir || nir->info.stage!=MESA_SHADER_COMPUTE || nir->info.workgroup_size[0]!=64 ||
       nir->info.workgroup_size[1]!=1 || nir->info.workgroup_size[2]!=1 ||
       nir->info.workgroup_size_variable || nir->info.shared_size || (nir->info.num_ssbos!=2 && nir->info.num_ssbos!=1))
      return SHIELD_NIR_METADATA;
   nir_function_impl *impl=nir_shader_get_entrypoint(nir);
   if (!impl || impl->ssa_alloc>4096) return SHIELD_NIR_LIMIT;
   unsigned functions=0,stores=0,instructions=0,blocks=0,written=0,budget=65536;
   nir_foreach_function(function,nir) if(function->impl) functions++;
   if(functions!=1) return SHIELD_NIR_CONTROL_FLOW;
   foreach_list_typed(nir_cf_node,node,node,&impl->body)
      if(node->type!=nir_cf_node_block || ++blocks!=1) return SHIELD_NIR_CONTROL_FLOW;
   foreach_list_typed(nir_cf_node,node,node,&impl->body) {
      nir_foreach_instr(instr,nir_cf_node_as_block(node)) {
         if (++instructions>1024) return SHIELD_NIR_LIMIT;
         switch(instr->type) {
         case nir_instr_type_load_const: break;
         case nir_instr_type_deref: {
            nir_deref_instr *d=nir_instr_as_deref(instr);
            if (d->modes!=nir_var_mem_ssbo && d->modes!=nir_var_system_value) return SHIELD_NIR_DEREF_MODE;
            break;
         }
         case nir_instr_type_alu: {
            nir_alu_instr *a=nir_instr_as_alu(instr);
            for(unsigned c=0;c<a->def.num_components;c++)
               if(evaluate(&a->def,c,0,&budget,contract,groups).form==INVALID) return budget?SHIELD_NIR_EXPRESSION:SHIELD_NIR_LIMIT;
            break;
         }
         case nir_instr_type_intrinsic: {
            nir_intrinsic_instr *i=nir_instr_as_intrinsic(instr);
            switch(i->intrinsic) {
            case nir_intrinsic_vulkan_resource_index:
            case nir_intrinsic_load_vulkan_descriptor: break;
            case nir_intrinsic_load_global_invocation_id:
            case nir_intrinsic_load_local_invocation_id:
            case nir_intrinsic_load_workgroup_id:
            case nir_intrinsic_load_deref:
               if (i->intrinsic==nir_intrinsic_load_deref && nir_intrinsic_access(i)&(ACCESS_VOLATILE|ACCESS_COHERENT)) return SHIELD_NIR_VOLATILE_ACCESS;
               for(unsigned c=0;c<i->def.num_components;c++)
                  if(evaluate(&i->def,c,0,&budget,contract,groups).form==INVALID) return budget?SHIELD_NIR_INPUT_ACCESS:SHIELD_NIR_LIMIT;
               break;
            case nir_intrinsic_store_deref: {
               bool scalar_store=contract==SHIELD_NIR_ZINK64_V1;
               unsigned width=scalar_store?1:4;
               if (++stores>(scalar_store?4:1) || i->src[1].ssa->num_components!=width ||
                   nir_intrinsic_write_mask(i)!=(scalar_store?1:15)) return SHIELD_NIR_STORE_SHAPE;
               if (nir_intrinsic_access(i)&(ACCESS_VOLATILE|ACCESS_COHERENT)) return SHIELD_NIR_VOLATILE_ACCESS;
               struct value index,row=constant(0);row.coefficient[1]=scalar_store?4:1;
               row.coefficient[6]=groups>1?64*row.coefficient[1]:0;
               if (!storage(nir_src_as_deref(i->src[0]),1,&index,0,&budget,contract,groups)) return SHIELD_NIR_OUTPUT_LAYOUT;
               unsigned first=scalar_store?index.coefficient[0]:0;
               if(first>3) return SHIELD_NIR_OUTPUT_LAYOUT;
               row.coefficient[0]=first;
               if(!equals(index,row)) return SHIELD_NIR_OUTPUT_LAYOUT;
               for(unsigned c=0;c<width;c++) {
                  unsigned component=first+c;
                  if(written&(1u<<component)) return SHIELD_NIR_STORE_SHAPE;
                  struct value equation=evaluate(i->src[1].ssa,c,0,&budget,contract,groups);
                  if(equation.form!=AFFINE || equation.coefficient[6]!=(groups>1?64*equation.coefficient[1]:0)) return budget?SHIELD_NIR_OUTPUT_EQUATION:SHIELD_NIR_LIMIT;
                  for(unsigned k=0;k<4;k++) {
                     uint32_t coefficient=equation.coefficient[2+k];
                     /* Interpret the modular representative without a narrowing signed cast.
                      * The existing FIELD_GEMM public-weight contract permits +/-119. */
                     if(coefficient<=119) transform->matrix[k][component]=(int32_t)coefficient;
                     else if(coefficient>=UINT32_MAX-118) transform->matrix[k][component]=-(int32_t)(0u-coefficient);
                     else return SHIELD_NIR_OUTPUT_EQUATION;
                  }
                  transform->bias[component]=equation.coefficient[0];
                  transform->row[component]=equation.coefficient[1];
                  written|=1u<<component;
               }
               break;
            }
            default: return SHIELD_NIR_INTRINSIC;
            }
            break;
         }
         case nir_instr_type_jump:
            if(nir_instr_as_jump(instr)->type!=nir_jump_return) return SHIELD_NIR_CONTROL_FLOW;
            break;
         default: return SHIELD_NIR_INSTRUCTION;
         }
      }
   }
   return written==15?SHIELD_NIR_OK:SHIELD_NIR_STORE_SHAPE;
}
struct shield_nir_proof shield_prepass_prove_nir(nir_shader *nir) {
   enum shield_nir_contract contract=nir && nir->info.num_ssbos==1?SHIELD_NIR_ZINK64_V1:SHIELD_NIR_FIXED64_V1;
   struct shield_affine64 transform={0};
   unsigned groups=64;
   enum shield_nir_reason reason=prove(nir,contract,&transform,groups);
   if(reason!=SHIELD_NIR_OK) {
      /* Preserve valid legacy single-group programs without broadening them. */
      groups=1; memset(&transform,0,sizeof transform);
      reason=prove(nir,contract,&transform,groups);
   }
   if(reason!=SHIELD_NIR_OK) return (struct shield_nir_proof){.contract=SHIELD_NIR_NONE,.reason=reason};
   if(!shield_affine64_is_fixed(&transform))
      contract=contract==SHIELD_NIR_ZINK64_V1?SHIELD_NIR_ZINK_AFFINE64_V2:SHIELD_NIR_AFFINE64_V2;
   return (struct shield_nir_proof){.contract=contract,.reason=reason,.transform=transform,.max_workgroups=groups};
}
const char *shield_nir_reason_name(enum shield_nir_reason reason) {
   const char *names[]={"ok","metadata","limit","control-flow","deref-mode","expression","input-access","volatile-access","store-shape","output-layout","output-equation","intrinsic","instruction"};
   return (unsigned)reason<ARRAY_SIZE(names)?names[reason]:"invalid-reason";
}
