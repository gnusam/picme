#!/bin/sh
# Genere les fixtures de test dans ce dossier. A lancer dans le conteneur dev :
#   docker compose exec -T dev tests/fixtures/make.sh
# Les fichiers produits ne sont pas versionnes (voir .gitignore).
set -eu
cd "$(dirname "$0")"

base=.base.png
# image deterministe (graine fixe) avec un rectangle rouge en haut a gauche : l'orientation se voit
magick -seed 1 -size 640x480 plasma:fractal -fill red -draw 'rectangle 0,0 120,80' -alpha off -depth 8 "$base"

# JPEG : 4:2:0, 640x480 (multiple de 16 : rotation sans perte parfaite), 333x217 (pas multiple du MCU), Orientation=6
magick "$base" -sampling-factor 4:2:0 -quality 90 a.jpg
magick "$base" -resize 333x217! -sampling-factor 4:2:0 -quality 90 b_333x217.jpg
magick "$base" -sampling-factor 4:2:0 -quality 90 c_orient6.jpg
exiftool -q -overwrite_original -n -Orientation=6 c_orient6.jpg

magick "$base" d.png
magick "$base" -compress lzw e.tiff
magick "$base" -quality 85 f.webp
heif-enc -q 80 "$base" -o g.heic
heif-enc -A -q 50 "$base" -o h.avif
cjxl -q 90 "$base" i.jxl

gst-launch-1.0 -q videotestsrc num-buffers=60 ! videoconvert ! avenc_mpeg4 ! qtmux ! filesink location=j.mov

rm -f "$base"
ls -l a.jpg b_333x217.jpg c_orient6.jpg d.png e.tiff f.webp g.heic h.avif i.jxl j.mov
