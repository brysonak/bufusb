# uefi-ntfs.img
UEFI firmware only knows how to read FAT natively, it has no built in NTFS support. But we need NTFS for the data partition because some files (e.g. install.wim/install.esd on windows images) exceed FAT32's 4 gig 1 file size cap

Thankfully, the saviour Pete Batard has us with this loader image, picked up from [here](https://github.com/pbatard/rufus/tree/master/res/uefi)

# exfat-upcase.bin
exFAT stores its own case-folding table on every volume. Taken from a volume made by exfatprogs' mkfs.exfat
