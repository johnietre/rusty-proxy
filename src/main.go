package main

import (
  "fmt"
  "io/ioutil"
  "net/http"

  "golang.org/x/net/hpack"
  "golang.org/x/net/http2"
)

func main() {
  server := http.Server{
    Addr: "127.0.0.1:8999",
    Handler: http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
      fmt.Println("Proto:", r.Proto)
      // If the request is send over http2, handle it as such
      if r.ProtoMajor == 2 {
        //
        return
      }
      fmt.Println("Headers:")
      for k, v := range r.Header {
        fmt.Printf("\t%s: %v\n", k, v)
      }
      fmt.Printf("URL: %s\n", r.URL)
      b, err := ioutil.ReadAll(r.Body)
      defer r.Body.Close()
      if err != nil {
        fmt.Println(err.Error())
      } else {
        fmt.Printf("Body: %s\n", b)
      }
    }),
  }
  server.ListenAndServe()
}
