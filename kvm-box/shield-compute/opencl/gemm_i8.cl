__kernel void gemm(__global const char *a, __global const char *b, __global long *c, uint M, uint N, uint K) {
 size_t row=get_global_id(0), col=get_global_id(1);
 if(row<M && col<N) {
  long sum=0;
  for(uint k=0;k<K;k++) sum+=(long)a[row*K+k]*(long)b[k*N+col];
  c[row*N+col]=sum;
 }
}
