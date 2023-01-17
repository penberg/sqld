# `sqld` User Guide

## Configuring gRPC to use TLS

Configure the Common Name (CN) of the server:

```console
export SERVER_CN=localhost
```

Generate a password that protects the keys:

```console
export PASSWORD=$(echo $RANDOM | shasum | head -c 20; echo)
```

Generate Certificate Authority private key (`ca.key`) and trust certificate (`ca.crt`):

```console
openssl genrsa -passout pass:$PASSWORD -des3 -out ca.key 4096
openssl req -passin pass:$PASSWORD -new -x509 -days 3650 -key ca.key -out ca.crt -subj "/CN=${SERVER_CN}"
```

Generate Server Private Key (`server.key`):

```console
openssl genrsa -passout pass:$PASSWORD -des3 -out server.key 4096
```

Generate Server Certificate Signing Request (`server.csr`):

```console
openssl req -passin pass:$PASSWORD -new -key server.key -out server.csr -subj "/CN=${SERVER_CN}" -config etc/ssl.cnf
```

Generate Server Certificate (`server.crt`):

```console
openssl x509 -req -passin pass:$PASSWORD -days 3650 -in server.csr -CA ca.crt -CAkey ca.key -set_serial 01 -out server.crt -extensions req_ext -extfile etc/ssl.cnf
```

Convert Server Certificate to `.pem` format (`server.pem`):

```console
openssl pkcs8 -topk8 -nocrypt -passin pass:$PASSWORD -in server.key -out server.pem
```

## Deploying to Fly

You can use the existing `fly.toml` file from this repository.

Just run
```console
flyctl launch
```
... then pick a name and respond "Yes" when the prompt asks you to deploy.

You now have `sqld` running on Fly listening for HTTP connections.

Give it a try with this snippet, replacing `$YOUR_APP` with your app name:
```
curl -X POST -d '{"statements": ["create table testme(a,b,c)"]}' $YOUR_APP.fly.dev
curl -X POST -d '{"statements": ["insert into testme values(1,2,3)"]}' $YOUR_APP.fly.dev
curl -X POST -d '{"statements": ["select * from testme"]}' $YOUR_APP.fly.dev
```
```
[{"b":2,"a":1,"c":3}]
```
