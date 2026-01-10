package com.example;

import org.mapstruct.Mapper;
import org.mapstruct.Mapping;
import org.mapstruct.MappingConstants;

@Mapper(
    componentModel = MappingConstants.ComponentModel.SPRING,
    implementationName = "<CLASS_NAME>CustomImpl",
    implementationPackage = "<PACKAGE_NAME>.generated"
)
public interface CarMapper {
    @Mapping(source = "numberOfSeats", target = "seatCount")
    CarDto carToCarDto(Car car);
}

