/* Variable-row transport oracle; synthetic service responses, no GPU claim. */
#include "shield_prepass_client.c"
#include <assert.h>

static void sentinel(const int32_t *p, size_t count) {
    for (size_t i=0;i<count*sizeof *p;i++) assert(((const unsigned char*)p)[i]==0xa5);
}
int main(void) {
    const unsigned counts[]={1,63,64,65,128,129,4095,4096};
    unsigned checks=0;
    for (unsigned test=0;test<sizeof counts/sizeof counts[0];test++) {
        unsigned n=counts[test];size_t words=(size_t)n*4,bytes=words*sizeof(uint32_t);
        struct shield_affine64 t=shield_affine64_fixed();
        t.matrix[0][0]=-119;t.matrix[2][3]=119;
        for(unsigned c=0;c<4;c++){t.bias[c]=UINT32_MAX-c;t.row[c]=0x80000000u+c;}
        uint32_t *input=malloc(bytes),*expected=malloc(bytes);
        int32_t *guarded=malloc(bytes+32),*output=guarded+4;
        size_t capacity=2048u+n*96u;
        char *body=malloc(capacity),*other=malloc(capacity);
        assert(input&&expected&&guarded&&body&&other);
        for(size_t i=0;i<words;i++) input[i]=(uint32_t)i*0x19318711u;
        memset(guarded,0xa5,bytes+32);
        size_t length=request(&t,input,n,body,capacity);assert(length && length<capacity);
        struct shield_affine64 changed=t;changed.bias[0]^=1;changed.row[3]^=UINT32_MAX;
        assert(request(&changed,input,n,other,capacity)==length && !strcmp(body,other));
        assert(!request(&t,input,n,other,32));
        struct json_object *req=json_tokener_parse(body),*graph,*nodes,*shape,*inputs,*x;
        assert(field(req,"graph",json_type_object,&graph));assert(field(graph,"nodes",json_type_array,&nodes));
        assert(field(json_object_array_get_idx(nodes,0),"shape",json_type_array,&shape));
        assert(json_object_get_int(json_object_array_get_idx(shape,0))==(int)n);
        assert(field(req,"inputs",json_type_object,&inputs));assert(field(inputs,"x",json_type_array,&x));
        assert(json_object_array_length(x)==n);
        for(unsigned r=0;r<n;r++)for(unsigned k=0;k<32;k++)
            assert(json_object_get_int(json_object_array_get_idx(json_object_array_get_idx(x,r),k))==(k<4?(int)(input[r*4+k]&255)-128:0));
        json_object_put(req);
        struct json_object *root=json_object_new_object(),*outputs=json_object_new_object(),*rows=json_object_new_array(),*routes=json_object_new_object(),*stats=json_object_new_object();
        json_object_object_add(root,"ok",json_object_new_boolean(true));
        json_object_object_add(root,"outputs",outputs);json_object_object_add(outputs,"y",rows);
        json_object_object_add(root,"routes",routes);json_object_object_add(routes,"y",json_object_new_string("masked-field-gemm"));
        json_object_object_add(root,"stats",stats);
        json_object_object_add(stats,"exchanges",json_object_new_int((n+63)/64));
        json_object_object_add(stats,"macs",json_object_new_int64((int64_t)n*128));
        json_object_object_add(stats,"verification_failures",json_object_new_int(0));
        for(unsigned r=0;r<n;r++) {
            struct json_object *row=json_object_new_array();json_object_array_add(rows,row);
            for(unsigned c=0;c<4;c++) {
                int64_t dot=0;for(unsigned k=0;k<4;k++)dot+=((int64_t)(input[r*4+k]&255)-128)*t.matrix[k][c];
                expected[r*4+c]=(uint32_t)((uint64_t)dot+t.bias[c]+(uint64_t)r*t.row[c]);
                json_object_array_add(row,json_object_new_int64(dot));
            }
        }
        const char *text=json_object_to_json_string_ext(root,JSON_C_TO_STRING_PLAIN);
        assert(response(&t,text,strlen(text),n,output)==SHIELD_PREPASS_MASKED);
        assert(!memcmp(output,expected,bytes));sentinel(guarded,4);sentinel(output+words,4);checks++;
        memset(output,0xa5,bytes);
        json_object_object_add(stats,"exchanges",json_object_new_int((n+63)/64+1));
        text=json_object_to_json_string_ext(root,JSON_C_TO_STRING_PLAIN);
        assert(response(&t,text,strlen(text),n,output)==SHIELD_PREPASS_FAILED);sentinel(guarded,words+8);checks++;
        json_object_object_add(stats,"exchanges",json_object_new_int((n+63)/64));
        json_object_object_add(stats,"macs",json_object_new_int64((int64_t)n*128-1));
        text=json_object_to_json_string_ext(root,JSON_C_TO_STRING_PLAIN);
        assert(response(&t,text,strlen(text),n,output)==SHIELD_PREPASS_FAILED);sentinel(guarded,words+8);checks++;
        json_object_object_del(stats,"exchanges");json_object_object_del(stats,"macs");json_object_object_del(stats,"verification_failures");
        json_object_object_add(routes,"y",json_object_new_string("local"));
        text=json_object_to_json_string_ext(root,JSON_C_TO_STRING_PLAIN);
        assert(response(&t,text,strlen(text),n,output)==SHIELD_PREPASS_LOCAL);assert(!memcmp(output,expected,bytes));checks++;
        memset(output,0xa5,bytes);
        json_object_array_put_idx(json_object_array_get_idx(rows,n-1),3,json_object_new_int64(60929));
        text=json_object_to_json_string_ext(root,JSON_C_TO_STRING_PLAIN);
        assert(response(&t,text,strlen(text),n,output)==SHIELD_PREPASS_FAILED);sentinel(guarded,words+8);checks++;
        json_object_put(root);free(body);free(other);free(input);free(expected);free(guarded);
    }
    /* Maximal decimal width at the public row/weight bounds. These exercise
     * the complete body, not only the capacity formula. Service framing is
     * independently capped at 8MiB, above this <400KiB client request. */
    {
        const unsigned n=4096;size_t words=n*4,bytes=words*sizeof(uint32_t);
        struct shield_affine64 t=shield_affine64_fixed();
        uint32_t *input=calloc(words,sizeof *input);int32_t *output=malloc(bytes);
        size_t capacity=2048+n*96;char *body=malloc(capacity);
        assert(input&&output&&body);
        for(unsigned k=0;k<4;k++)for(unsigned c=0;c<4;c++)t.matrix[k][c]=-119;
        size_t length=request(&t,input,n,body,capacity);
        assert(length>300000 && length<capacity && length<(8u<<20));checks++;
        for(unsigned sign=0;sign<2;sign++) {
            int dot=sign?60928:-60928;
            for(unsigned k=0;k<4;k++)for(unsigned c=0;c<4;c++)t.matrix[k][c]=sign?-119:119;
            struct json_object *root=json_object_new_object(),*outputs=json_object_new_object(),*rows=json_object_new_array(),*routes=json_object_new_object(),*stats=json_object_new_object();
            json_object_object_add(root,"ok",json_object_new_boolean(true));
            json_object_object_add(root,"outputs",outputs);json_object_object_add(outputs,"y",rows);
            json_object_object_add(root,"routes",routes);json_object_object_add(routes,"y",json_object_new_string("masked-field-gemm"));
            json_object_object_add(root,"stats",stats);
            json_object_object_add(stats,"exchanges",json_object_new_int(64));
            json_object_object_add(stats,"macs",json_object_new_int(524288));
            json_object_object_add(stats,"verification_failures",json_object_new_int(0));
            for(unsigned r=0;r<n;r++) {
                struct json_object *row=json_object_new_array();json_object_array_add(rows,row);
                for(unsigned c=0;c<4;c++)json_object_array_add(row,json_object_new_int(dot));
            }
            const char *text=json_object_to_json_string_ext(root,JSON_C_TO_STRING_PLAIN);
            assert(strlen(text)<2048+n*32);
            assert(response(&t,text,strlen(text),n,output)==SHIELD_PREPASS_MASKED);
            for(unsigned i=0;i<words;i++)assert(output[i]==dot);
            checks++;json_object_put(root);
        }
        memset(output,0xa5,bytes);memset(body,' ',2048+n*32+1);
        assert(response(&t,body,2048+n*32+1,n,output)==SHIELD_PREPASS_FAILED);
        sentinel(output,words);checks++;free(body);free(output);free(input);
    }
    struct shield_affine64 t=shield_affine64_fixed();uint32_t input[256]={0};int32_t output[256];memset(output,0xa5,sizeof output);
    for(unsigned n=0;n<=1;n++) { assert(shield_prepass_execute_rows(&t,input,n?4097:0,output)==SHIELD_PREPASS_FAILED);sentinel(output,256);checks++; }
    printf("grid client: %u request/counter/bounds/atomic-publication checks passed\n",checks);
}
