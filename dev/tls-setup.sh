#!/bin/sh
# Runs inside the database containers just before the server starts: a throwaway CA and a server
# certificate, so TLS can be tested with real certificate checks.
#
#   tls-setup.sh <owner> <name> <names>
#     owner  user:group that must be able to read the key (mysql:mysql, postgres:postgres)
#     name   the CA is copied to dev/certs/<name>.pem for the tests to trust
#     names  what the certificate names, as subjectAltName entries: DNS:localhost,IP:127.0.0.1
set -e
dir=/etc/dbtop-tls
mkdir -p "$dir" /certs-out
cd "$dir"

if [ ! -f server.pem ]; then
  openssl req -x509 -newkey rsa:2048 -nodes -keyout ca.key -out ca.pem -days 3650 \
    -subj "/CN=dbtop dev CA" \
    -addext "basicConstraints=critical,CA:TRUE" -addext "keyUsage=critical,keyCertSign,cRLSign"
  openssl req -newkey rsa:2048 -nodes -keyout server.key -out server.csr -subj "/CN=localhost"
  cat > server.ext <<EXT
basicConstraints = CA:FALSE
keyUsage = digitalSignature, keyEncipherment
extendedKeyUsage = serverAuth
subjectAltName = $3
EXT
  # Under 825 days: macOS refuses longer-lived server certificates.
  openssl x509 -req -in server.csr -CA ca.pem -CAkey ca.key -CAcreateserial \
    -out server.pem -days 800 -extfile server.ext
fi

chown "$1" ca.pem server.pem server.key
chmod 644 ca.pem server.pem
chmod 600 server.key
cp ca.pem "/certs-out/$2.pem"
chmod 644 "/certs-out/$2.pem"
