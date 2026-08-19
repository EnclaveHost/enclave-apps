package main
import ("bytes";"fmt";"os";"github.com/ipni/go-libipni/announce/message")
func main(){
  b,_:=os.ReadFile(os.Args[1])
  var m message.Message
  if err:=m.UnmarshalCBOR(bytes.NewReader(b));err!=nil{panic(err)}
  addrs,_:=m.GetAddrs()
  fmt.Println("announce head cid:",m.Cid.String())
  fmt.Println("announce addrs:",addrs)
  fmt.Println("PASS: go-libipni parsed the app's announce message")
}
