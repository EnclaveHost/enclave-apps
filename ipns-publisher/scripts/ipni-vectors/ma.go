package main
import ma "github.com/multiformats/go-multiaddr"
func mustMultiaddr(s string) []byte {
	m, err := ma.NewMultiaddr(s)
	if err != nil { panic(err) }
	return m.Bytes()
}
