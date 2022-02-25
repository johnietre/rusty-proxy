package main

import (
  "log"
  "net/http"
  "os"
)

var logger = log.New(os.Stdout, "", log.LstdFlags)

func main() {
  saddrPtr := flag.String("server", "localhost:9990", "Server address")
  paddrPtr := flag.String("proxy", "localhost:10000", "Proxy address")
  pathPtr := flag.String("path", "/", "Path")
  methodPtr := flag.String("method", "GET", "Method of the request")
  flag.Parse()

  s := &http.Server{
    Addr: *saddrPtr,
    Handler: handler,
    ErrorLog: logger,
  }
  go s.ListenAndServe()
  defer s.Shutdown()

  reqURL, method := *paddrPtr + *pathPtr, strings.ToUpper(*methodPtr)
  var resp *http.Response
  var err error
  switch method
  case "GET":
    resp, err = http.Get(reqURL)
  default:
    logger.Fatalf("invalid method: %s", method)
  }
  if err != nil {
    logger.Printf("%s -> %s request received error: %v", reqURL, method, err)
  } else {
    logger.Printf("%s -> %s request received code: %d", reqURL, method, resp.StatusCode)
  }
}

func handler(w http.ResponseWriter, r *http.Request) {
  logger.Printf("Received %s method from %s", r.Method, r.URL)
  logger.Printf("Headers:")
  for key, values := range r.Headers {
    logger.Printf("%s: %v", key, values)
  }
}
