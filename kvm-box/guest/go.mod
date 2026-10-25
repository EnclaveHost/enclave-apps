module enclave.host/kvm-box/guest

go 1.24

require enclave.host/isolation/m2 v0.0.0
require enclave.host/isolation/contract v0.0.0
replace enclave.host/isolation/m2 => ../../../enclave/isolation/m2
replace enclave.host/isolation/contract => ../../../enclave/isolation/contract
