find_library(GMP_LIBRARY NAMES gmp )
include(FindPackageHandleStandardArgs)
find_package_handle_standard_args(GMP DEFAULT_MSG GMP_LIBRARY)
