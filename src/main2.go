package main

import (
  "fmt"
  "io"
  "net"

  "golang.org/x/net/http2"
  "golang.org/x/net/http2/hpack"
)

func main() {
  ln, err := net.Listen("tcp", "127.0.0.1:8999")
  if err != nil {
    panic(err)
  }
  defer ln.Close()
  for {
    conn, _ := ln.Accept()
    go handle(conn)
  }
}

func handle(conn net.Conn) {
  defer conn.Close()
  framer := http2.NewFramer(io.Discard, conn)
  frame, _ := framer.ReadFrame()
  fmt.Println(frame.Header().Type)
  fmt.Println(frame.Header().Type)
  return
  hef := frame.(*http2.HeadersFrame)
  d := hpack.NewDecoder(5000, nil)
  hf, err := d.DecodeFull(hef.HeaderBlockFragment())
  if err != nil {
    panic(err)
  }
  for _, h := range hf {
    fmt.Printf("%s:%s\n", h.Name, h.Value)
  }
}
