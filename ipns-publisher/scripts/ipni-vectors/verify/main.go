package main

import (
	"bytes"
	"fmt"
	"io"
	"net/http"
	"os"

	"github.com/ipfs/go-cid"
	cidlink "github.com/ipld/go-ipld-prime/linking/cid"
	"github.com/ipni/go-libipni/dagsync/ipnisync/head"
	"github.com/ipni/go-libipni/ingest/schema"
	"github.com/multiformats/go-multihash"
)

// Crawl an ipni-sync HTTP publisher exactly as cid.contact would, using
// go-libipni to parse+verify every block. Prints PASS/FAIL.
func main() {
	base := os.Args[1] // e.g. http://127.0.0.1:18540
	get := func(p string) []byte {
		r, err := http.Get(base + p)
		if err != nil {
			panic(err)
		}
		defer r.Body.Close()
		if r.StatusCode != 200 {
			panic(fmt.Sprintf("GET %s -> %d", p, r.StatusCode))
		}
		b, _ := io.ReadAll(r.Body)
		return b
	}

	// 1. head
	sh, err := head.Decode(bytes.NewReader(get("/ipni/v1/ad/head")))
	if err != nil {
		panic("head decode: " + err.Error())
	}
	pid, err := sh.Validate()
	if err != nil {
		panic("head signature INVALID: " + err.Error())
	}
	headCid := sh.Head.(interface{ String() string }).String()
	fmt.Println("HEAD signature valid, provider (from head pubkey):", pid.String())
	fmt.Println("HEAD ad cid:", headCid)

	// 2. walk the ad chain, verifying every ad's signature
	adCidStr := headCid
	seen := 0
	var entriesCid cid.Cid
	for adCidStr != "" && !(adCidStr == cid.Undef.String()) {
		c, err := cid.Decode(adCidStr)
		if err != nil {
			panic("bad ad cid: " + err.Error())
		}
		adBytes := get("/ipni/v1/ad/" + adCidStr)
		ad, err := schema.BytesToAdvertisement(c, adBytes)
		if err != nil {
			panic("ad parse: " + err.Error())
		}
		signer, err := ad.VerifySignature()
		if err != nil {
			panic(fmt.Sprintf("ad %s signature INVALID: %s", adCidStr, err))
		}
		fmt.Printf("AD %s: signature valid, signer=%s provider=%s addrs=%v metadata=%x isRm=%v\n",
			adCidStr, signer.String(), ad.Provider, ad.Addresses, ad.Metadata, ad.IsRm)
		if signer.String() != ad.Provider {
			panic("signer != provider")
		}
		entriesCid = ad.Entries.(cidlink.Link).Cid
		seen++
		if ad.PreviousID == nil {
			break
		}
		adCidStr = ad.PreviousID.(interface{ String() string }).String()
	}
	fmt.Printf("chain verified: %d ad(s)\n", seen)

	// 3. entry chunk: parse the multihashes
	ecBytes := get("/ipni/v1/ad/" + entriesCid.String())
	ec, err := schema.BytesToEntryChunk(entriesCid, ecBytes)
	if err != nil {
		panic("entry chunk parse: " + err.Error())
	}
	fmt.Printf("ENTRY CHUNK %s: %d multihashes\n", entriesCid.String(), len(ec.Entries))
	// print each as a CID (raw codec) for comparison with kubo, and as mh
	for _, mh := range ec.Entries {
		_, err := multihash.Decode(mh)
		if err != nil {
			panic("bad multihash in entry chunk")
		}
		fmt.Println("MH", mh.HexString())
	}
	fmt.Println("PASS: go-libipni accepts the ad chain served by the app")
}
