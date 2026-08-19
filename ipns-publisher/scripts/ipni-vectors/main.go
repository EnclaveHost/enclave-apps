package main

import (
	"bytes"
	"crypto/ed25519"
	"encoding/hex"
	"fmt"

	"github.com/ipfs/go-cid"
	ipld "github.com/ipld/go-ipld-prime"
	"github.com/ipld/go-ipld-prime/codec/dagcbor"
	"github.com/ipld/go-ipld-prime/codec/dagjson"
	cidlink "github.com/ipld/go-ipld-prime/linking/cid"
	"github.com/libp2p/go-libp2p/core/crypto"
	"github.com/libp2p/go-libp2p/core/peer"
	"github.com/multiformats/go-multihash"

	"github.com/ipni/go-libipni/announce/message"
	"github.com/ipni/go-libipni/dagsync/ipnisync/head"
	"github.com/ipni/go-libipni/ingest/schema"
)

func must(err error) {
	if err != nil {
		panic(err)
	}
}

func dagcborBytesCid(n ipld.Node) ([]byte, cid.Cid) {
	b, err := ipld.Encode(n, dagcbor.Encode)
	must(err)
	mh, err := multihash.Sum(b, multihash.SHA2_256, -1)
	must(err)
	return b, cid.NewCidV1(cid.DagCBOR, mh)
}

func main() {
	// deterministic ed25519 key: seed = 32 x 0x11 (same as the Rust test)
	seed := bytes.Repeat([]byte{0x11}, 32)
	edPriv := ed25519.NewKeyFromSeed(seed)
	priv, err := crypto.UnmarshalEd25519PrivateKey(edPriv)
	must(err)
	pid, err := peer.IDFromPublicKey(priv.GetPublic())
	must(err)
	pubMarshaled, err := crypto.MarshalPublicKey(priv.GetPublic())
	must(err)

	fmt.Println("SEED", hex.EncodeToString(seed))
	fmt.Println("PROVIDER", pid.String())
	fmt.Println("PUBKEY_MARSHALED", hex.EncodeToString(pubMarshaled))

	// --- EntryChunk: two known multihashes ---
	mh1, _ := multihash.Sum([]byte("block-one"), multihash.SHA2_256, -1)
	mh2, _ := multihash.Sum([]byte("block-two"), multihash.SHA2_256, -1)
	ec := schema.EntryChunk{
		Entries: []multihash.Multihash{mh1, mh2},
	}
	ecNode, err := ec.ToNode()
	must(err)
	ecBytes, ecCid := dagcborBytesCid(ecNode)
	fmt.Println("MH1", hex.EncodeToString(mh1))
	fmt.Println("MH2", hex.EncodeToString(mh2))
	fmt.Println("ENTRYCHUNK_DAGCBOR", hex.EncodeToString(ecBytes))
	fmt.Println("ENTRYCHUNK_CID", ecCid.String())

	// --- Advertisement (first in chain: PreviousID nil) ---
	metadata := []byte{0xA0, 0x12} // uvarint(0x0920), transport-ipfs-gateway-http
	ctxID := []byte("site-root-context")
	ad := schema.Advertisement{
		Provider:  pid.String(),
		Addresses: []string{"/dns4/ipfs.enclave.host/tcp/443/https"},
		Entries:   cidlink.Link{Cid: ecCid},
		ContextID: ctxID,
		Metadata:  metadata,
		IsRm:      false,
	}
	must(ad.Sign(priv))
	fmt.Println("AD_SIGNATURE", hex.EncodeToString(ad.Signature))
	adNode, err := ad.ToNode()
	must(err)
	adBytes, adCid := dagcborBytesCid(adNode)
	fmt.Println("AD_DAGCBOR", hex.EncodeToString(adBytes))
	fmt.Println("AD_CID", adCid.String())

	// verify our understanding round-trips
	must(ad.Validate())
	_, err = schema.BytesToAdvertisement(adCid, adBytes)
	must(err)

	// --- Second advertisement, chained to the first ---
	ad2 := schema.Advertisement{
		PreviousID: cidlink.Link{Cid: adCid},
		Provider:   pid.String(),
		Addresses:  []string{"/dns4/ipfs.enclave.host/tcp/443/https"},
		Entries:    cidlink.Link{Cid: ecCid},
		ContextID:  ctxID,
		Metadata:   metadata,
		IsRm:       false,
	}
	must(ad2.Sign(priv))
	ad2Node, err := ad2.ToNode()
	must(err)
	ad2Bytes, ad2Cid := dagcborBytesCid(ad2Node)
	fmt.Println("AD2_SIGNATURE", hex.EncodeToString(ad2.Signature))
	fmt.Println("AD2_DAGCBOR", hex.EncodeToString(ad2Bytes))
	fmt.Println("AD2_CID", ad2Cid.String())

	// --- SignedHead over ad2Cid (dag-json), default topic ---
	sh, err := head.NewSignedHead(ad2Cid, "", priv)
	must(err)
	shNode, err := sh.ToNode()
	must(err)
	var shBuf bytes.Buffer
	must(dagjson.Encode(shNode, &shBuf))
	fmt.Println("SIGNEDHEAD_DAGJSON", hex.EncodeToString(shBuf.Bytes()))

	// --- Announce message (PUT /announce body), CBOR tuple ---
	msg := message.Message{
		Cid: ad2Cid,
	}
	// publisher addr: this app's own https endpoint
	msg.Addrs = [][]byte{mustMultiaddr("/dns4/pub.example/tcp/443/https")}
	var mbuf bytes.Buffer
	must(msg.MarshalCBOR(&mbuf))
	fmt.Println("ANNOUNCE_CBOR", hex.EncodeToString(mbuf.Bytes()))
}
