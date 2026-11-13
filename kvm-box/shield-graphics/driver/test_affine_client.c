#define LEGACY_TEST_CAPACITY 16384
/* Independent integer oracle for the generalized transport; synthetic replies only. */
#include "shield_prepass_client.c"
#include <assert.h>
static uint32_t seed=0x6284ab3f;
static uint32_t next(void) {seed^=seed<<13;seed^=seed>>17;seed^=seed<<5;return seed;}
static void unchanged(const int32_t out[256]) {
    for(unsigned i=0;i<sizeof(int32_t)*256;i++) assert(((const unsigned char*)out)[i]==0xa5);
}
int main(void) {
    unsigned cases=0;
    for(unsigned trial=0;trial<256;trial++) {
        struct shield_affine64 t={0};uint32_t input[256],expected[256];int32_t output[256];
        for(unsigned k=0;k<4;k++) for(unsigned c=0;c<4;c++)
            t.matrix[k][c]=trial<2?(trial==0?-119:119):(int32_t)(next()%239)-119;
        for(unsigned c=0;c<4;c++) { t.bias[c]=next();t.row[c]=next(); }
        for(unsigned i=0;i<256;i++) input[i]=next();
        assert(shield_affine64_valid(&t));
        char body[LEGACY_TEST_CAPACITY],same[LEGACY_TEST_CAPACITY];assert(request(&t,input,64,body,LEGACY_TEST_CAPACITY));
        struct shield_affine64 changed=t;
        for(unsigned c=0;c<4;c++){changed.bias[c]^=0xffffffffu;changed.row[c]^=0xaaaaaaaa;}
        assert(request(&changed,input,64,same,LEGACY_TEST_CAPACITY));assert(!strcmp(body,same));
        struct json_object *req=json_tokener_parse(body), *graph,*nodes,*weights,*values;
        assert(field(req,"graph",json_type_object,&graph));assert(field(graph,"nodes",json_type_array,&nodes));
        assert(json_object_array_length(nodes)==3);
        weights=json_object_array_get_idx(nodes,1);assert(field(weights,"value",json_type_array,&values));
        for(unsigned k=0;k<32;k++) for(unsigned c=0;c<4;c++)
            assert(json_object_get_int(json_object_array_get_idx(json_object_array_get_idx(values,k),c))==(k<4?t.matrix[k][c]:0));
        struct json_object *inputs,*input_rows;
        assert(field(req,"inputs",json_type_object,&inputs));assert(field(inputs,"x",json_type_array,&input_rows));
        assert(json_object_array_length(input_rows)==64);
        for(unsigned r=0;r<64;r++) {
            struct json_object *row=json_object_array_get_idx(input_rows,r);assert(json_object_array_length(row)==32);
            for(unsigned k=0;k<32;k++)assert(json_object_get_int(json_object_array_get_idx(row,k))==(k<4?(int)(input[r*4+k]&255)-128:0));
        }
        json_object_put(req);
        struct json_object *root=json_object_new_object(),*outputs=json_object_new_object(),*rows=json_object_new_array();
        struct json_object *routes=json_object_new_object(),*stats=json_object_new_object();
        json_object_object_add(root,"ok",json_object_new_boolean(true));
        json_object_object_add(root,"outputs",outputs);json_object_object_add(outputs,"y",rows);
        json_object_object_add(root,"routes",routes);json_object_object_add(routes,"y",json_object_new_string("masked-field-gemm"));
        json_object_object_add(root,"stats",stats);
        json_object_object_add(stats,"exchanges",json_object_new_int(1));
        json_object_object_add(stats,"macs",json_object_new_int(8192));
        json_object_object_add(stats,"verification_failures",json_object_new_int(0));
        for(unsigned r=0;r<64;r++) {
            struct json_object *row=json_object_new_array();json_object_array_add(rows,row);
            for(unsigned c=0;c<4;c++) {
                int64_t value=0;
                for(unsigned k=0;k<4;k++) value+=((int64_t)(input[r*4+k]%256)-128)*t.matrix[k][c];
                expected[r*4+c]=(uint32_t)((uint64_t)value+t.bias[c]+(uint64_t)r*t.row[c]);
                json_object_array_add(row,json_object_new_int64(value));
            }
        }
        const char *text=json_object_to_json_string_ext(root,JSON_C_TO_STRING_PLAIN);
        assert(response(&t,text,strlen(text),64,output)==SHIELD_PREPASS_MASKED);
        assert(!memcmp(output,expected,sizeof expected));cases++;
        json_object_object_add(routes,"y",json_object_new_string("local"));
        json_object_object_del(stats,"exchanges");json_object_object_del(stats,"macs");json_object_object_del(stats,"verification_failures");
        text=json_object_to_json_string_ext(root,JSON_C_TO_STRING_PLAIN);
        assert(response(&t,text,strlen(text),64,output)==SHIELD_PREPASS_LOCAL);
        assert(!memcmp(output,expected,sizeof expected));cases++;
        /* Fail at the final element, after 255 valid values; publish nothing. */
        json_object_array_put_idx(json_object_array_get_idx(rows,63),3,json_object_new_int(60929));
        text=json_object_to_json_string_ext(root,JSON_C_TO_STRING_PLAIN);memset(output,0xa5,sizeof output);
        assert(response(&t,text,strlen(text),64,output)==SHIELD_PREPASS_FAILED);unchanged(output);cases++;
        json_object_array_put_idx(json_object_array_get_idx(rows,63),3,json_object_new_int(-60929));
        text=json_object_to_json_string_ext(root,JSON_C_TO_STRING_PLAIN);
        assert(response(&t,text,strlen(text),64,output)==SHIELD_PREPASS_FAILED);unchanged(output);cases++;
        json_object_put(root);
    }
    struct shield_affine64 t=shield_affine64_fixed();uint32_t input[256]={0};int32_t output[256];
    for(unsigned i=0;i<4;i++) {
        t.matrix[0][0]=(int32_t[]){-120,120,INT32_MIN,INT32_MAX}[i];memset(output,0xa5,sizeof output);
        assert(!shield_affine64_valid(&t));assert(shield_prepass_execute_transform(&t,input,output)==SHIELD_PREPASS_FAILED);
        unchanged(output);cases++;
    }
    printf("affine client: %u synthetic oracle/bounds/rejection cases passed; no GPU claim\n",cases);
}
