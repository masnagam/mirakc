set -eu

BASEDIR=$(cd $(dirname $0); pwd)
TARGET=$1
BUILDPLATFORM=$2
TARGETPLATFORM=$3

. $BASEDIR/vars.sh

MIRAKC_ARIB_VERSION='0.12.0'
ARCHIVE="https://github.com/mirakc/mirakc-arib/archive/$MIRAKC_ARIB_VERSION.tar.gz"

curl -fsSL $ARCHIVE | tar -xz --strip-components=1

MIRAKC_ARIB_TSDUCK_ARIB_CXXFLAGS=''
if [ "$TARGET" = alpine ]; then
  # See https://gist.github.com/uru2/cb3f7b553c2c58570ca9bf18e47cebb3
  MIRAKC_ARIB_TSDUCK_ARIB_CXXFLAGS='-Wno-error=zero-as-null-pointer-constant'
  if [ "$TARGETPLATFORM" = linux/386 ]; then
    # Disable SSP in order solve link errors.
    # See https://bugs.gentoo.org/706210
    MIRAKC_ARIB_TSDUCK_ARIB_CXXFLAGS="$MIRAKC_ARIB_TSDUCK_ARIB_CXXFLAGS -fno-stack-protector"
  fi
fi

cat <<EOF >toolchain.cmake
set(CMAKE_SYSTEM_NAME Linux)
set(CMAKE_SYSTEM_PROCESSOR $GCC_ARCH)
set(CMAKE_C_COMPILER $GCC)
set(CMAKE_C_COMPILER_TARGET $GCC_HOST_TRIPLE)
set(CMAKE_CXX_COMPILER $GXX)
set(CMAKE_CXX_COMPILER_TARGET $GCC_HOST_TRIPLE)
set(MIRAKC_ARIB_TSDUCK_ARIB_CXXFLAGS "$MIRAKC_ARIB_TSDUCK_ARIB_CXXFLAGS" CACHE STRING "" FORCE)
EOF

cmake -B. -S. -DCMAKE_BUILD_TYPE=Release -DCMAKE_TOOLCHAIN_FILE=toolchain.cmake
make -j $(nproc) vendor
make -j $(nproc)
